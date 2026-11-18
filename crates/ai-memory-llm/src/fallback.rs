//! Ordered LLM provider fallback chain.
//!
//! See `docs/llm-provider-fallback.md` (issue #648) for the full design.
//! `FallbackLlmProvider` wraps an ordered list of already-constructed
//! providers behind the same [`LlmProvider`] trait: on a transient failure
//! (`LlmError::is_transient()`) or exhausted credit/quota
//! (`LlmError::is_quota_exhausted()`) it advances to the next eligible
//! candidate, preserving the original request, schema, and operation id; a
//! deterministic failure (bad request, auth, unsupported schema, malformed
//! response) returns immediately without trying the rest of the chain.
//!
//! A transient failure keeps its candidate out for [`CIRCUIT_COOLDOWN`] (or
//! the provider's retry hint); exhausted credit or quota keeps it out for the
//! longer quota cooldown ([`DEFAULT_QUOTA_COOLDOWN`] unless configured). When
//! the cooldown ends, the next real call probes that candidate first again,
//! so a topped-up primary takes over without a restart and without any
//! background traffic.
//!
//! `Config::llm_provider_chain` in `ai-memory-cli` is the sole place that
//! builds one; `build_provider` remains the sole construction path for each
//! candidate.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jiff::Timestamp;
use tracing::{debug, info, warn};

use crate::error::{FailoverClass, LlmError, LlmResult};
use crate::health::CandidateHealth;
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse, LlmOperationId};

/// How long a candidate's circuit stays open after a transient failure
/// before it is eligible again. Restarting the process clears all circuit
/// state; there is no durable or cross-process cooldown.
pub const CIRCUIT_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_CIRCUIT_COOLDOWN: Duration = Duration::from_secs(60 * 60);
/// How long a candidate whose provider reported exhausted credit, quota or
/// billing stays out of the chain before the next call probes it again.
/// `llm_quota_cooldown_secs` in `ai-memory-cli` overrides it.
pub const DEFAULT_QUOTA_COOLDOWN: Duration = Duration::from_secs(15 * 60);

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
    /// Why the circuit was last opened, for health reporting.
    circuit_reason: Option<FailoverClass>,
}

struct CandidateEntry {
    candidate: Candidate,
    state: Mutex<CandidateState>,
}

/// Ordered LLM provider fallback chain.
///
/// Tries each eligible candidate in declaration order, advancing only on a
/// transient failure or exhausted credit/quota; a deterministic failure
/// returns immediately. See the
/// module docs and `docs/llm-provider-fallback.md`.
pub struct FallbackLlmProvider {
    entries: Vec<CandidateEntry>,
    /// Index of the candidate that answered the most recently completed
    /// call, for health reporting.
    last_selected: Mutex<Option<usize>>,
    /// Circuit cooldown after exhausted credit or quota.
    quota_cooldown: Duration,
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
            quota_cooldown: DEFAULT_QUOTA_COOLDOWN,
        }
    }

    /// Override how long a candidate that reported exhausted credit or quota
    /// is skipped before the next call probes it again
    /// ([`DEFAULT_QUOTA_COOLDOWN`] by default).
    #[must_use]
    pub const fn with_quota_cooldown(mut self, cooldown: Duration) -> Self {
        self.quota_cooldown = cooldown;
        self
    }

    /// The circuit cooldown a failure earns, or `None` when it does not
    /// advance the chain. A transient failure gets [`CIRCUIT_COOLDOWN`],
    /// stretched by a provider retry hint up to one hour. Exhausted quota
    /// gets the quota cooldown, stretched by a longer hint (a quota reset
    /// time) up to one hour or the quota cooldown, whichever is longer.
    fn cooldown_for(&self, err: &LlmError) -> Option<(FailoverClass, Duration)> {
        match err.failover_class() {
            FailoverClass::Transient => Some((
                FailoverClass::Transient,
                err.retry_after()
                    .unwrap_or(CIRCUIT_COOLDOWN)
                    .max(CIRCUIT_COOLDOWN)
                    .min(MAX_CIRCUIT_COOLDOWN),
            )),
            FailoverClass::QuotaExhausted => Some((
                FailoverClass::QuotaExhausted,
                err.retry_after()
                    .unwrap_or(self.quota_cooldown)
                    .max(self.quota_cooldown)
                    .min(MAX_CIRCUIT_COOLDOWN.max(self.quota_cooldown)),
            )),
            FailoverClass::Permanent => None,
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
            state.circuit_reason = None;
        }
        let previous = self
            .last_selected
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .replace(index);
        // Logged on a change of answering candidate only, so a primary that
        // stays down for an hour yields one line instead of one per call.
        let switched = match previous {
            Some(previous) => previous != index,
            None => index > 0,
        };
        if switched {
            let candidate = &self.entries[index].candidate;
            info!(
                provider = candidate.provider,
                model = %candidate.model,
                position = index,
                "llm fallback chain now answering from this candidate"
            );
        }
    }

    fn record_error(&self, index: usize, err: &LlmError) {
        let outcome = self.cooldown_for(err);
        let cooldown = outcome.map(|(_, cooldown)| cooldown);
        let should_log_transient = {
            let mut state = self.entries[index]
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let circuit_was_open =
                matches!(state.circuit_open_until, Some(until) if Instant::now() < until);
            state.last_error_at = Some(Timestamp::now());
            state.last_error_status = err.http_status();
            state.last_error_class = Some(err.class());
            if let Some((class, cooldown)) = outcome {
                // Overlapping failures keep the later deadline, and the
                // reason follows whichever failure set it: a short transient
                // blip must not shorten or relabel a quota cooldown.
                let deadline = Instant::now() + cooldown;
                if state
                    .circuit_open_until
                    .is_none_or(|until| deadline >= until)
                {
                    state.circuit_open_until = Some(deadline);
                    state.circuit_reason = Some(class);
                }
            }
            cooldown.is_some() && !circuit_was_open
        };
        // Redacted fields only, as the reranker does: the `Display` of a
        // provider failure carries the upstream response body. The model is
        // the failing candidate's, which the caller cannot see: every caller
        // labels the chain with its first candidate.
        let candidate = &self.entries[index].candidate;
        match outcome {
            Some((FailoverClass::QuotaExhausted, cooldown)) if should_log_transient => warn!(
                provider = candidate.provider,
                model = %candidate.model,
                position = index,
                error_class = err.class(),
                error_status = ?err.http_status(),
                cooldown_secs = cooldown.as_secs(),
                "llm fallback candidate is out of credit or quota; skipping it until \
                 its quota cooldown ends, then probing it again"
            ),
            Some((_, cooldown)) if should_log_transient => warn!(
                provider = candidate.provider,
                model = %candidate.model,
                position = index,
                error_class = err.class(),
                error_status = ?err.http_status(),
                cooldown_secs = cooldown.as_secs(),
                "llm fallback candidate failed; skipping it until its cooldown ends"
            ),
            Some(_) => {}
            None => debug!(
                provider = candidate.provider,
                model = %candidate.model,
                position = index,
                error_class = err.class(),
                error_status = ?err.http_status(),
                "llm fallback candidate failed deterministically; the chain stops here"
            ),
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

    /// Summary entry for a candidate skipped because its circuit is open, so
    /// an aggregate error with every circuit open still names the reason.
    fn describe_skipped(&self, index: usize) -> String {
        let candidate = &self.entries[index].candidate;
        let reason = self.entries[index]
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .circuit_reason
            .map_or("transient", FailoverClass::as_str);
        format!(
            "{}/{}: skipped (circuit open: {reason})",
            candidate.provider, candidate.model
        )
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
                summary.push(self.describe_skipped(i));
                continue;
            }
            attempted += 1;
            match entry.candidate.inner.complete(request.clone()).await {
                Ok(response) => {
                    self.record_success(i);
                    return Ok(response);
                }
                Err(err) => {
                    let fail_over = err.should_fail_over();
                    summary.push(self.describe_failure(i, &err));
                    self.record_error(i, &err);
                    if !fail_over {
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
                summary.push(self.describe_skipped(i));
                continue;
            }
            attempted += 1;
            match entry
                .candidate
                .inner
                .complete_with_operation_id(request.clone(), operation_id.clone())
                .await
            {
                Ok(response) => {
                    self.record_success(i);
                    return Ok(response);
                }
                Err(err) => {
                    let fail_over = err.should_fail_over();
                    summary.push(self.describe_failure(i, &err));
                    self.record_error(i, &err);
                    if !fail_over {
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
                summary.push(self.describe_skipped(i));
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
                    let fail_over = err.should_fail_over();
                    summary.push(self.describe_failure(i, &err));
                    self.record_error(i, &err);
                    if !fail_over {
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
                summary.push(self.describe_skipped(i));
                continue;
            }
            attempted += 1;
            match entry
                .candidate
                .inner
                .complete_structured_raw_with_operation_id(
                    request.clone(),
                    schema.clone(),
                    operation_id.clone(),
                )
                .await
            {
                Ok(value) => {
                    self.record_success(i);
                    return Ok(value);
                }
                Err(err) => {
                    let fail_over = err.should_fail_over();
                    summary.push(self.describe_failure(i, &err));
                    self.record_error(i, &err);
                    if !fail_over {
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
                    circuit_reason: state
                        .circuit_open_until
                        .filter(|until| *until > now)
                        .and(state.circuit_reason)
                        .map(|reason| reason.as_str().to_string()),
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
            LlmError::Provider {
                status,
                body,
                retry_after,
            } => LlmError::Provider {
                status: *status,
                body: body.clone(),
                retry_after: *retry_after,
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
            retry_after: None,
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
            .complete_with_operation_id(request.clone(), op_id.clone())
            .await
            .unwrap();
        chain
            .complete_structured_raw(request.clone(), schema.clone())
            .await
            .unwrap();
        chain
            .complete_structured_raw_with_operation_id(
                request.clone(),
                schema.clone(),
                op_id.clone(),
            )
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
        assert_eq!(
            *seen_ops,
            vec![None, Some(op_id.clone()), None, Some(op_id)]
        );
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
                retry_after: None,
            },
            LlmError::Provider {
                status: 401,
                body: String::new(),
                retry_after: None,
            },
            LlmError::Provider {
                status: 403,
                body: String::new(),
                retry_after: None,
            },
            LlmError::Provider {
                status: 404,
                body: String::new(),
                retry_after: None,
            },
            LlmError::Provider {
                status: 422,
                body: String::new(),
                retry_after: None,
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
            retry_after: None,
        }
    }

    #[test]
    fn provider_retry_hints_are_clamped_to_the_circuit_bounds() {
        let chain = FallbackLlmProvider::new(vec![Candidate::new(
            "primary",
            "m1",
            Arc::new(ScriptedLlm::ok("primary", "m1", "unused")),
        )]);

        for (hint, expected) in [(5, 30), (90, 90), (7_200, 3_600)] {
            let before = Instant::now();
            chain.record_error(
                0,
                &LlmError::Provider {
                    status: 429,
                    body: String::new(),
                    retry_after: Some(Duration::from_secs(hint)),
                },
            );
            let until = chain.entries[0]
                .state
                .lock()
                .unwrap()
                .circuit_open_until
                .unwrap();
            let actual = until.duration_since(before);
            assert!(
                actual >= Duration::from_secs(expected)
                    && actual < Duration::from_secs(expected + 1),
                "hint {hint}s produced {actual:?}"
            );
        }
    }

    #[test]
    fn overlapping_transient_failures_preserve_the_longer_cooldown() {
        let chain = FallbackLlmProvider::new(vec![Candidate::new(
            "primary",
            "m1",
            Arc::new(ScriptedLlm::ok("primary", "m1", "unused")),
        )]);
        chain.record_error(
            0,
            &LlmError::Provider {
                status: 429,
                body: String::new(),
                retry_after: Some(Duration::from_secs(1_800)),
            },
        );
        let deadline = chain.entries[0].state.lock().unwrap().circuit_open_until;

        // Another request was already in flight when the first failure
        // opened the circuit. Its default cooldown is shorter.
        chain.record_error(0, &transient(503));

        assert_eq!(
            chain.entries[0].state.lock().unwrap().circuit_open_until,
            deadline
        );
    }

    #[test]
    fn a_transient_failure_does_not_shorten_or_relabel_a_quota_cooldown() {
        let chain = FallbackLlmProvider::new(vec![Candidate::new(
            "primary",
            "m1",
            Arc::new(ScriptedLlm::ok("primary", "m1", "unused")),
        )]);
        chain.record_error(0, &quota(400, ANTHROPIC_CREDIT_BODY));
        let (deadline, reason) = {
            let state = chain.entries[0].state.lock().unwrap();
            (state.circuit_open_until, state.circuit_reason)
        };
        assert_eq!(reason, Some(FailoverClass::QuotaExhausted));

        // A request already in flight fails transiently afterwards.
        chain.record_error(0, &transient(503));

        let state = chain.entries[0].state.lock().unwrap();
        assert_eq!(state.circuit_open_until, deadline);
        assert_eq!(state.circuit_reason, Some(FailoverClass::QuotaExhausted));
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

    const ANTHROPIC_CREDIT_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}"#;
    const OPENAI_QUOTA_BODY: &str = r#"{"error":{"message":"You exceeded your current quota, please check your plan and billing details.","type":"insufficient_quota","param":null,"code":"insufficient_quota"}}"#;

    fn quota(status: u16, body: &str) -> LlmError {
        LlmError::Provider {
            status,
            body: body.to_string(),
            retry_after: None,
        }
    }

    fn remaining_cooldown(chain: &FallbackLlmProvider, index: usize) -> Duration {
        chain.entries[index]
            .state
            .lock()
            .unwrap()
            .circuit_open_until
            .expect("circuit open")
            .saturating_duration_since(Instant::now())
    }

    #[tokio::test]
    async fn exhausted_credit_fails_over_and_keeps_the_primary_out_for_the_quota_cooldown() {
        let primary = Arc::new(ScriptedLlm::err(
            "anthropic",
            "claude",
            quota(400, ANTHROPIC_CREDIT_BODY),
        ));
        let fallback = Arc::new(ScriptedLlm::ok("openai", "gpt", "from openai"));
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new("anthropic", "claude", primary.clone()),
            Candidate::new("openai", "gpt", fallback.clone()),
        ]);

        for _ in 0..3 {
            let response = chain
                .complete(ChatRequest::user_prompt("hi"))
                .await
                .expect("the fallback answers while the primary is out of credit");
            assert_eq!(response.text, "from openai");
        }
        // One probe, then the primary is skipped for the whole quota window.
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 3);
        let remaining = remaining_cooldown(&chain, 0);
        assert!(
            remaining > DEFAULT_QUOTA_COOLDOWN - Duration::from_secs(5)
                && remaining <= DEFAULT_QUOTA_COOLDOWN,
            "{remaining:?}"
        );

        let health = chain.candidate_health();
        assert_eq!(health[0].last_error_status, Some(400));
        assert_eq!(
            health[0].last_error_class.as_deref(),
            Some("quota-exhausted")
        );
        assert_eq!(health[0].circuit_reason.as_deref(), Some("quota-exhausted"));
        assert!(health[1].last_selected);
        assert!(health[1].circuit_reason.is_none());
        assert!(!format!("{health:?}").contains("credit balance"));
    }

    #[tokio::test]
    async fn every_quota_signal_advances_regardless_of_status() {
        for err in [
            quota(400, ANTHROPIC_CREDIT_BODY),
            quota(429, OPENAI_QUOTA_BODY),
            quota(
                402,
                r#"{"error":{"message":"Insufficient credits","code":402}}"#,
            ),
            quota(
                403,
                r#"{"type":"error","error":{"type":"permission_error","message":"This organization has been disabled."}}"#,
            ),
        ] {
            let status = err.http_status();
            let first = Arc::new(ScriptedLlm::err("primary", "m1", err));
            let second = Arc::new(ScriptedLlm::ok("secondary", "m2", "ok"));
            let chain = FallbackLlmProvider::new(vec![
                Candidate::new("primary", "m1", first.clone()),
                Candidate::new("secondary", "m2", second.clone()),
            ]);
            chain
                .complete_structured_raw(
                    ChatRequest::user_prompt("hi"),
                    serde_json::json!({ "type": "object" }),
                )
                .await
                .unwrap_or_else(|e| panic!("{status:?} must fail over: {e}"));
            assert_eq!(second.calls(), 1, "{status:?}");
        }
    }

    #[tokio::test]
    async fn the_primary_is_probed_again_after_the_quota_cooldown_and_takes_back_over() {
        let primary = Arc::new(ScriptedLlm::new(
            "anthropic",
            "claude",
            vec![
                Err(quota(400, ANTHROPIC_CREDIT_BODY)),
                Err(quota(400, ANTHROPIC_CREDIT_BODY)),
                Ok(ChatResponse {
                    text: "from anthropic".into(),
                    usage: None,
                    model: "claude".into(),
                }),
            ],
        ));
        let fallback = Arc::new(ScriptedLlm::ok("openai", "gpt", "from openai"));
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new("anthropic", "claude", primary.clone()),
            Candidate::new("openai", "gpt", fallback.clone()),
        ])
        .with_quota_cooldown(Duration::from_secs(600));
        let expire = |chain: &FallbackLlmProvider| {
            chain.entries[0].state.lock().unwrap().circuit_open_until =
                Some(Instant::now() - Duration::from_millis(1));
        };

        let first = chain.complete(ChatRequest::user_prompt("1")).await.unwrap();
        assert_eq!(first.text, "from openai");
        // Inside the window: no traffic to the primary.
        chain.complete(ChatRequest::user_prompt("2")).await.unwrap();
        assert_eq!(primary.calls(), 1);

        // Window over, still out of credit: one probe, then back out for
        // another full window.
        expire(&chain);
        let probe = chain.complete(ChatRequest::user_prompt("3")).await.unwrap();
        assert_eq!(probe.text, "from openai");
        assert_eq!(primary.calls(), 2);
        assert!(remaining_cooldown(&chain, 0) > Duration::from_secs(590));

        // Window over and topped up: the primary answers and its circuit closes.
        expire(&chain);
        let back = chain.complete(ChatRequest::user_prompt("4")).await.unwrap();
        assert_eq!(back.text, "from anthropic");
        assert_eq!(primary.calls(), 3);
        assert_eq!(fallback.calls(), 3);
        let health = chain.candidate_health();
        assert!(health[0].last_selected);
        assert!(health[0].circuit_open_until.is_none());
        assert!(health[0].circuit_reason.is_none());
    }

    #[test]
    fn a_quota_reset_hint_stretches_the_quota_cooldown_but_never_shortens_it() {
        let chain = FallbackLlmProvider::new(vec![Candidate::new(
            "primary",
            "m1",
            Arc::new(ScriptedLlm::ok("primary", "m1", "unused")),
        )])
        .with_quota_cooldown(Duration::from_secs(900));
        for (hint, expected) in [
            (None, 900),
            (Some(5), 900),
            (Some(2_400), 2_400),
            (Some(90_000), 3_600),
        ] {
            chain.record_error(
                0,
                &LlmError::Provider {
                    status: 429,
                    body: OPENAI_QUOTA_BODY.into(),
                    retry_after: hint.map(Duration::from_secs),
                },
            );
            let actual = remaining_cooldown(&chain, 0);
            assert!(
                actual > Duration::from_secs(expected - 2)
                    && actual <= Duration::from_secs(expected),
                "hint {hint:?} produced {actual:?}"
            );
        }
        // A configured window longer than an hour is honoured as-is.
        let long = FallbackLlmProvider::new(vec![Candidate::new(
            "primary",
            "m1",
            Arc::new(ScriptedLlm::ok("primary", "m1", "unused")),
        )])
        .with_quota_cooldown(Duration::from_secs(7_200));
        long.record_error(0, &quota(400, ANTHROPIC_CREDIT_BODY));
        assert!(remaining_cooldown(&long, 0) > Duration::from_secs(7_190));
    }

    #[tokio::test]
    async fn a_real_permanent_error_after_a_quota_failover_still_stops_the_chain() {
        let primary = Arc::new(ScriptedLlm::err(
            "anthropic",
            "claude",
            quota(400, ANTHROPIC_CREDIT_BODY),
        ));
        let fallback = Arc::new(ScriptedLlm::err(
            "openai",
            "gpt",
            quota(
                400,
                r#"{"error":{"message":"Invalid schema for response_format","type":"invalid_request_error","code":null}}"#,
            ),
        ));
        let spare = Arc::new(ScriptedLlm::ok("gemini", "flash", "unreachable"));
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new("anthropic", "claude", primary),
            Candidate::new("openai", "gpt", fallback.clone()),
            Candidate::new("gemini", "flash", spare.clone()),
        ]);
        let err = chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .expect_err("a malformed request must not be retried elsewhere");
        assert_eq!(err.class(), "provider");
        assert_eq!(err.http_status(), Some(400));
        assert_eq!(fallback.calls(), 1);
        assert_eq!(spare.calls(), 0);
    }

    #[tokio::test]
    async fn a_generic_invalid_key_does_not_fail_over() {
        let primary = Arc::new(ScriptedLlm::err(
            "anthropic",
            "claude",
            quota(
                401,
                r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
            ),
        ));
        let fallback = Arc::new(ScriptedLlm::ok("openai", "gpt", "unreachable"));
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new("anthropic", "claude", primary),
            Candidate::new("openai", "gpt", fallback.clone()),
        ]);
        let err = chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .expect_err("an invalid key is the operator's to fix");
        assert_eq!(err.http_status(), Some(401));
        assert_eq!(fallback.calls(), 0);
    }

    #[tokio::test]
    async fn every_candidate_out_of_quota_returns_a_clear_redacted_aggregate() {
        let primary = Arc::new(ScriptedLlm::err(
            "anthropic",
            "claude",
            quota(400, ANTHROPIC_CREDIT_BODY),
        ));
        let fallback = Arc::new(ScriptedLlm::err(
            "openai",
            "gpt",
            quota(429, OPENAI_QUOTA_BODY),
        ));
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new("anthropic", "claude", primary.clone()),
            Candidate::new("openai", "gpt", fallback.clone()),
        ]);

        let err = chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .expect_err("nothing is left to answer");
        let LlmError::AllCandidatesFailed { attempted, summary } = &err else {
            panic!("expected AllCandidatesFailed, got {err}");
        };
        assert_eq!(*attempted, 2);
        assert!(
            summary.contains("anthropic/claude: quota-exhausted 400"),
            "{summary}"
        );
        assert!(
            summary.contains("openai/gpt: quota-exhausted 429"),
            "{summary}"
        );
        let rendered = err.to_string();
        assert!(!rendered.contains("credit balance"), "{rendered}");
        assert!(!rendered.contains("insufficient_quota"), "{rendered}");
        assert!(!err.is_transient());

        // While both windows are open nothing is sent, and the error still
        // says why instead of an empty summary.
        let err = chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .expect_err("both circuits are open");
        assert_eq!(primary.calls(), 1);
        assert_eq!(fallback.calls(), 1);
        let LlmError::AllCandidatesFailed { attempted, summary } = &err else {
            panic!("expected AllCandidatesFailed, got {err}");
        };
        assert_eq!(*attempted, 0);
        assert!(
            summary.contains("anthropic/claude: skipped (circuit open: quota-exhausted)"),
            "{summary}"
        );
        assert!(
            summary.contains("openai/gpt: skipped (circuit open: quota-exhausted)"),
            "{summary}"
        );
    }

    /// Collects what a subscriber writes so a test can assert on log lines.
    #[derive(Clone, Default)]
    struct CapturedLog(Arc<Mutex<Vec<u8>>>);

    impl CapturedLog {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for CapturedLog {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn info_subscriber(writer: CapturedLog) -> impl tracing::Subscriber + Send + Sync {
        tracing_subscriber::fmt()
            .with_writer(writer)
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .finish()
    }

    /// Captures this thread's log lines. tracing-core caches each callsite's
    /// interest process-wide, so a parallel test reaching the same `warn!`
    /// with no subscriber can pin it to `never` and drop the line here. As in
    /// `ai-memory-mcp`'s `install_capture`, a second live `Dispatch` stops that
    /// and `rebuild_interest_cache()` heals a callsite already pinned. The
    /// companion must outlive the capture, so the caller holds it.
    fn capture_info_logs() -> (
        CapturedLog,
        tracing::subscriber::DefaultGuard,
        tracing::Dispatch,
    ) {
        let captured = CapturedLog::default();
        let companion = tracing::Dispatch::new(info_subscriber(CapturedLog::default()));
        let guard = tracing::subscriber::set_default(info_subscriber(captured.clone()));
        tracing::callsite::rebuild_interest_cache();
        (captured, guard, companion)
    }

    #[tokio::test]
    async fn a_transient_failure_logs_the_failing_candidate_without_its_body() {
        let (log, _guard, _companion) = capture_info_logs();
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new(
                "openai-compat",
                "gemini-3.8-flash",
                Arc::new(ScriptedLlm::err(
                    "openai-compat",
                    "gemini-3.8-flash",
                    LlmError::Provider {
                        status: 429,
                        body: "quota body sk-secret-echo".into(),
                        retry_after: Some(Duration::from_secs(600)),
                    },
                )),
            ),
            Candidate::new(
                "openai-compat",
                "gemini-3.6-flash",
                Arc::new(ScriptedLlm::ok("openai-compat", "gemini-3.6-flash", "ok")),
            ),
        ]);

        chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .unwrap();

        let text = log.text();
        let failure = text
            .lines()
            .find(|line| line.contains("skipping it until its cooldown ends"))
            .expect("the transient failure is logged");
        assert!(failure.contains("gemini-3.8-flash"), "{failure}");
        assert!(failure.contains("429"), "{failure}");
        assert!(failure.contains("cooldown_secs=600"), "{failure}");
        assert!(!text.contains("sk-secret-echo"), "the body leaked: {text}");
        assert!(
            text.lines()
                .any(|line| line.contains("now answering") && line.contains("gemini-3.6-flash")),
            "the switch to the fallback is logged: {text}"
        );
    }

    #[tokio::test]
    async fn a_quota_failover_logs_the_reason_and_cooldown_without_the_body() {
        let (log, _guard, _companion) = capture_info_logs();
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new(
                "anthropic",
                "claude-sonnet",
                Arc::new(ScriptedLlm::err(
                    "anthropic",
                    "claude-sonnet",
                    quota(400, ANTHROPIC_CREDIT_BODY),
                )),
            ),
            Candidate::new(
                "openai",
                "gpt-mini",
                Arc::new(ScriptedLlm::ok("openai", "gpt-mini", "ok")),
            ),
        ]);

        chain
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .unwrap();

        let text = log.text();
        let line = text
            .lines()
            .find(|line| line.contains("out of credit or quota"))
            .expect("the quota failover is logged");
        assert!(line.contains("claude-sonnet"), "{line}");
        assert!(line.contains("quota-exhausted"), "{line}");
        assert!(line.contains("cooldown_secs=900"), "{line}");
        assert!(!text.contains("credit balance"), "the body leaked: {text}");
    }

    #[tokio::test]
    async fn the_answering_candidate_is_logged_only_when_it_changes() {
        let (log, _guard, _companion) = capture_info_logs();
        let chain = FallbackLlmProvider::new(vec![
            Candidate::new(
                "gemini",
                "primary",
                Arc::new(ScriptedLlm::err(
                    "gemini",
                    "primary",
                    LlmError::Provider {
                        status: 503,
                        body: "overloaded".into(),
                        retry_after: None,
                    },
                )),
            ),
            Candidate::new(
                "gemini",
                "spare",
                Arc::new(ScriptedLlm::ok("gemini", "spare", "ok")),
            ),
        ]);

        // The primary's circuit stays open across these calls, so each one is
        // answered by the spare: one switch, then silence.
        for _ in 0..3 {
            chain
                .complete(ChatRequest::user_prompt("hi"))
                .await
                .unwrap();
        }
        assert_eq!(log.text().matches("now answering").count(), 1);

        // Control: a chain whose primary answers never logs a switch.
        let (quiet_log, _quiet_guard, _quiet_companion) = capture_info_logs();
        let healthy = FallbackLlmProvider::new(vec![Candidate::new(
            "gemini",
            "primary",
            Arc::new(ScriptedLlm::ok("gemini", "primary", "ok")),
        )]);
        healthy
            .complete(ChatRequest::user_prompt("hi"))
            .await
            .unwrap();
        assert!(!quiet_log.text().contains("now answering"));
    }

    #[test]
    fn concurrent_transient_failures_log_only_the_circuit_transition() {
        let (log, _guard, _companion) = capture_info_logs();
        let chain = FallbackLlmProvider::new(vec![Candidate::new(
            "gemini",
            "primary",
            Arc::new(ScriptedLlm::ok("gemini", "primary", "unused")),
        )]);
        let failure = LlmError::Provider {
            status: 503,
            body: "must not leak".into(),
            retry_after: Some(Duration::from_secs(600)),
        };

        // Two requests can both reach the provider before the first failure
        // opens its circuit. The later result still refreshes health state,
        // but it must not repeat the outage warning.
        chain.record_error(0, &failure);
        chain.record_error(0, &failure);

        let text = log.text();
        assert_eq!(
            text.matches("skipping it until its cooldown ends").count(),
            1,
            "an already-open circuit repeated its warning: {text}"
        );
        assert!(!text.contains("must not leak"), "the body leaked: {text}");
    }
}
