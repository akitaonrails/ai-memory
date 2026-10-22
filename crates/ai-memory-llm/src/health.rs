//! Passive provider-health recording wrappers.
//!
//! These wrappers never probe providers on their own. They only record
//! the result of calls the server was already going to make.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::embedding::Embedder;
use crate::error::{LlmError, LlmResult};
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse, LlmOperationId, LlmWorkload, Usage};

const MAX_ERROR_MESSAGE_CHARS: usize = 1024;

/// Process-scoped health recorder for the configured provider roles.
#[derive(Clone, Default)]
pub struct ProviderHealth {
    llm: ProviderRoleHealth,
    llm_workloads: LlmWorkloadHealth,
    embedding: ProviderRoleHealth,
    /// The configured LLM provider, held only so `snapshot()` can read its
    /// [`LlmProvider::candidate_health`] — a plain provider always returns
    /// empty here, a [`crate::fallback::FallbackLlmProvider`] returns its
    /// ordered candidate list. Never dereferenced for anything but that
    /// read, so this adds no behavior beyond passive reporting.
    llm_provider: Arc<Mutex<Option<Arc<dyn LlmProvider>>>>,
}

impl ProviderHealth {
    /// Return a serializable snapshot of the latest recorded state.
    #[must_use]
    pub fn snapshot(&self) -> ProviderHealthSnapshot {
        let llm_candidates = self
            .llm_provider
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|provider| provider.candidate_health())
            .unwrap_or_default();
        let llm = self.llm.snapshot();
        let llm_workloads = if llm.status == ProviderHealthStatus::Disabled {
            Vec::new()
        } else {
            self.llm_workloads.snapshot()
        };
        ProviderHealthSnapshot {
            llm,
            llm_candidates,
            llm_workloads,
            embedding: self.embedding.snapshot(),
        }
    }

    /// Mark the LLM role as configured and wrap it with passive recording.
    #[must_use]
    pub fn wrap_llm_provider(
        &self,
        inner: Arc<dyn LlmProvider>,
        provider: impl Into<String>,
        model: impl Into<String>,
        retry_hint: Option<String>,
    ) -> Arc<dyn LlmProvider> {
        let health = self.llm.clone();
        health.configure(provider.into(), model.into(), None, retry_hint);
        self.llm_workloads.reset();
        *self.llm_provider.lock().unwrap_or_else(|e| e.into_inner()) = Some(inner.clone());
        Arc::new(HealthRecordingLlmProvider {
            inner,
            health,
            workloads: self.llm_workloads.clone(),
        })
    }

    /// Mark the embedding role as configured and wrap it with passive recording.
    #[must_use]
    pub fn wrap_embedder(
        &self,
        inner: Arc<dyn Embedder>,
        provider: impl Into<String>,
        model: impl Into<String>,
        dim: u32,
    ) -> Arc<dyn Embedder> {
        let health = self.embedding.clone();
        health.configure(provider.into(), model.into(), Some(dim), None);
        Arc::new(HealthRecordingEmbedder { inner, health })
    }
}

/// Wire-format provider-health status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderHealthStatus {
    /// The role has no provider configured.
    #[default]
    Disabled,
    /// The role is configured, but no provider call has happened in this process.
    Unknown,
    /// The last provider call succeeded.
    Ok,
    /// The last provider call failed.
    Error,
}

impl ProviderHealthStatus {
    /// Canonical kebab-case wire string.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Unknown => "unknown",
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

/// Wire-format health snapshot for all provider roles.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealthSnapshot {
    /// LLM provider role.
    pub llm: ProviderRoleHealthSnapshot,
    /// Ordered LLM fallback-chain candidate state (`llm_fallbacks`), in
    /// declaration order (index 0 is the primary provider). Empty for a
    /// single configured provider or no provider at all.
    /// `#[serde(default)]` so a response from a server built before this
    /// field existed still deserializes for a newer CLI.
    #[serde(default)]
    pub llm_candidates: Vec<CandidateHealth>,
    /// Passive per-workload LLM counts, latency, token estimates, reported
    /// usage and last outcome. Empty when no LLM provider is configured.
    #[serde(default)]
    pub llm_workloads: Vec<LlmWorkloadHealthSnapshot>,
    /// Embedding provider role.
    pub embedding: ProviderRoleHealthSnapshot,
}

/// Passive process-lifetime telemetry for one closed LLM workload label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LlmWorkloadHealthSnapshot {
    /// Why ai-memory made these calls.
    pub workload: LlmWorkload,
    /// Provider-interface invocations, including errors. Candidate attempts
    /// inside one fallback-chain invocation remain one call; an outer caller
    /// that deliberately retries makes another invocation.
    pub calls: u64,
    /// Successful provider-interface invocations.
    pub successes: u64,
    /// Failed provider-interface invocations.
    pub errors: u64,
    /// Total wall time across calls, in microseconds.
    pub latency_total_us: u64,
    /// Slowest observed call, in microseconds.
    pub latency_max_us: u64,
    /// Upper bound of the bucket containing p50. `None` means no calls or a
    /// value above the largest fixed bucket (10 seconds).
    pub latency_p50_upper_us: Option<u64>,
    /// Upper bound of the bucket containing p95, with the same `None`
    /// semantics as p50.
    pub latency_p95_upper_us: Option<u64>,
    /// Coarse four-characters-per-token input estimate for every call.
    pub estimated_input_tokens: u64,
    /// Coarse four-characters-per-token output estimate for successful calls.
    pub estimated_output_tokens: u64,
    /// Provider-reported input tokens when the provider returned usage.
    pub reported_input_tokens: u64,
    /// Provider-reported output tokens when the provider returned usage.
    pub reported_output_tokens: u64,
    /// Calls for which the provider returned token usage. Structured-output
    /// providers currently return JSON without usage, so estimates remain the
    /// comparable measurement for those workloads.
    pub usage_reported_calls: u64,
    /// Outcome of the most recent call, or `unknown` before the first call.
    pub status: ProviderHealthStatus,
    /// Time of the most recent call.
    pub last_call_at: Option<Timestamp>,
    /// Time of the most recent success.
    pub last_success_at: Option<Timestamp>,
    /// Time of the most recent error.
    pub last_error_at: Option<Timestamp>,
    /// Redacted error class for the most recent error.
    pub last_error_class: Option<String>,
    /// HTTP status for the most recent error, when one existed.
    pub last_error_status: Option<u16>,
}

/// One candidate's passive health state in an ordered LLM fallback chain.
/// See `docs/llm-provider-fallback.md`. Every entry here is by construction
/// a configured chain member; redaction is total — only labels, timestamps,
/// an HTTP status, and a short error class are recorded, never a response
/// body or credential.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateHealth {
    /// Candidate provider label.
    pub provider: String,
    /// Candidate model id.
    pub model: String,
    /// Whether this candidate answered the most recently completed chain
    /// call.
    pub last_selected: bool,
    /// Timestamp of the last call this candidate answered successfully.
    pub last_success_at: Option<Timestamp>,
    /// Timestamp of the last call this candidate failed.
    pub last_error_at: Option<Timestamp>,
    /// HTTP status captured from the last error, when available.
    pub last_error_status: Option<u16>,
    /// Redacted error class captured from the last error
    /// ([`LlmError::class`]), never a response body or secret.
    pub last_error_class: Option<String>,
    /// When this candidate's circuit reopens, if a transient failure has it
    /// currently open. `None` once the cooldown has elapsed, even if no
    /// later call has explicitly closed it.
    pub circuit_open_until: Option<Timestamp>,
}

/// Wire-format health snapshot for one provider role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderRoleHealthSnapshot {
    /// Current role status.
    pub status: ProviderHealthStatus,
    /// Configured provider label, when the role is enabled.
    pub provider: Option<String>,
    /// Configured model, when the role is enabled.
    pub model: Option<String>,
    /// Configured embedding dimensionality. Only set for the embedding role.
    pub dim: Option<u32>,
    /// Timestamp of the last provider call, successful or failed.
    pub last_call_at: Option<Timestamp>,
    /// Timestamp of the last successful provider call.
    pub last_success_at: Option<Timestamp>,
    /// Timestamp of the last failed provider call.
    pub last_error_at: Option<Timestamp>,
    /// HTTP status captured from the last error, when available.
    pub last_error_status: Option<u16>,
    /// Truncated message captured from the last error, when available.
    pub last_error_message: Option<String>,
    /// Manual retry command hint. Set only for LLM provider errors.
    pub retry_hint: Option<String>,
}

impl Default for ProviderRoleHealthSnapshot {
    fn default() -> Self {
        Self {
            status: ProviderHealthStatus::Disabled,
            provider: None,
            model: None,
            dim: None,
            last_call_at: None,
            last_success_at: None,
            last_error_at: None,
            last_error_status: None,
            last_error_message: None,
            retry_hint: None,
        }
    }
}

#[derive(Clone)]
struct ProviderRoleHealth {
    state: Arc<Mutex<ProviderRoleState>>,
}

impl Default for ProviderRoleHealth {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(ProviderRoleState::disabled())),
        }
    }
}

impl ProviderRoleHealth {
    fn configure(
        &self,
        provider: String,
        model: String,
        dim: Option<u32>,
        retry_hint: Option<String>,
    ) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) =
            ProviderRoleState::configured(provider, model, dim, retry_hint);
    }

    fn snapshot(&self) -> ProviderRoleHealthSnapshot {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .snapshot()
    }

    fn record_result<T>(&self, result: &LlmResult<T>) {
        match result {
            Ok(_) => self.record_success(),
            Err(err) => self.record_error(err),
        }
    }

    fn record_success(&self) {
        let now = Timestamp::now();
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.status = ProviderHealthStatus::Ok;
        state.last_call_at = Some(now);
        state.last_success_at = Some(now);
        state.last_error_at = None;
        state.last_error_status = None;
        state.last_error_message = None;
    }

    fn record_error(&self, err: &LlmError) {
        let now = Timestamp::now();
        let (status, message) = error_status_and_message(err);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.status = ProviderHealthStatus::Error;
        state.last_call_at = Some(now);
        state.last_error_at = Some(now);
        state.last_error_status = status;
        state.last_error_message = Some(message);
    }
}

#[derive(Clone)]
struct ProviderRoleState {
    status: ProviderHealthStatus,
    provider: Option<String>,
    model: Option<String>,
    dim: Option<u32>,
    last_call_at: Option<Timestamp>,
    last_success_at: Option<Timestamp>,
    last_error_at: Option<Timestamp>,
    last_error_status: Option<u16>,
    last_error_message: Option<String>,
    retry_hint: Option<String>,
}

impl ProviderRoleState {
    fn disabled() -> Self {
        Self {
            status: ProviderHealthStatus::Disabled,
            provider: None,
            model: None,
            dim: None,
            last_call_at: None,
            last_success_at: None,
            last_error_at: None,
            last_error_status: None,
            last_error_message: None,
            retry_hint: None,
        }
    }

    fn configured(
        provider: String,
        model: String,
        dim: Option<u32>,
        retry_hint: Option<String>,
    ) -> Self {
        Self {
            status: ProviderHealthStatus::Unknown,
            provider: Some(provider),
            model: Some(model),
            dim,
            last_call_at: None,
            last_success_at: None,
            last_error_at: None,
            last_error_status: None,
            last_error_message: None,
            retry_hint,
        }
    }

    fn snapshot(&self) -> ProviderRoleHealthSnapshot {
        ProviderRoleHealthSnapshot {
            status: self.status,
            provider: self.provider.clone(),
            model: self.model.clone(),
            dim: self.dim,
            last_call_at: self.last_call_at,
            last_success_at: self.last_success_at,
            last_error_at: self.last_error_at,
            last_error_status: self.last_error_status,
            last_error_message: self.last_error_message.clone(),
            retry_hint: if self.status == ProviderHealthStatus::Error {
                self.retry_hint.clone()
            } else {
                None
            },
        }
    }
}

const LLM_WORKLOAD_BUCKETS: usize = ai_memory_core::LATENCY_BUCKET_BOUNDS_US.len() + 1;

#[derive(Clone)]
struct LlmWorkloadHealth {
    state: Arc<Mutex<[LlmWorkloadState; LlmWorkload::ALL.len()]>>,
}

impl Default for LlmWorkloadHealth {
    fn default() -> Self {
        Self {
            state: Arc::new(Mutex::new(std::array::from_fn(|_| {
                LlmWorkloadState::default()
            }))),
        }
    }
}

impl LlmWorkloadHealth {
    fn reset(&self) {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) =
            std::array::from_fn(|_| LlmWorkloadState::default());
    }

    fn record<T>(
        &self,
        workload: LlmWorkload,
        elapsed: Duration,
        estimated_input_tokens: u64,
        result: &LlmResult<T>,
        estimated_output_tokens: impl FnOnce(&T) -> u64,
        reported_usage: impl FnOnce(&T) -> Option<Usage>,
    ) {
        let now = Timestamp::now();
        let elapsed_us = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        let mut states = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut states[workload.index()];
        state.calls = state.calls.saturating_add(1);
        state.latency_total_us = state.latency_total_us.saturating_add(elapsed_us);
        state.latency_max_us = state.latency_max_us.max(elapsed_us);
        let bucket = ai_memory_core::LATENCY_BUCKET_BOUNDS_US
            .iter()
            .position(|bound| elapsed_us <= *bound)
            .unwrap_or(LLM_WORKLOAD_BUCKETS - 1);
        state.latency_buckets[bucket] = state.latency_buckets[bucket].saturating_add(1);
        state.estimated_input_tokens = state
            .estimated_input_tokens
            .saturating_add(estimated_input_tokens);
        state.last_call_at = Some(now);
        match result {
            Ok(output) => {
                state.successes = state.successes.saturating_add(1);
                state.estimated_output_tokens = state
                    .estimated_output_tokens
                    .saturating_add(estimated_output_tokens(output));
                if let Some(usage) = reported_usage(output) {
                    state.usage_reported_calls = state.usage_reported_calls.saturating_add(1);
                    state.reported_input_tokens = state
                        .reported_input_tokens
                        .saturating_add(u64::from(usage.input_tokens));
                    state.reported_output_tokens = state
                        .reported_output_tokens
                        .saturating_add(u64::from(usage.output_tokens));
                }
                state.status = ProviderHealthStatus::Ok;
                state.last_success_at = Some(now);
            }
            Err(error) => {
                state.errors = state.errors.saturating_add(1);
                state.status = ProviderHealthStatus::Error;
                state.last_error_at = Some(now);
                state.last_error_class = Some(error.class().to_owned());
                state.last_error_status = error.http_status();
            }
        }
    }

    fn snapshot(&self) -> Vec<LlmWorkloadHealthSnapshot> {
        let states = self.state.lock().unwrap_or_else(|e| e.into_inner());
        LlmWorkload::ALL
            .iter()
            .copied()
            .map(|workload| states[workload.index()].snapshot(workload))
            .collect()
    }
}

#[derive(Clone)]
struct LlmWorkloadState {
    calls: u64,
    successes: u64,
    errors: u64,
    latency_buckets: [u64; LLM_WORKLOAD_BUCKETS],
    latency_total_us: u64,
    latency_max_us: u64,
    estimated_input_tokens: u64,
    estimated_output_tokens: u64,
    reported_input_tokens: u64,
    reported_output_tokens: u64,
    usage_reported_calls: u64,
    status: ProviderHealthStatus,
    last_call_at: Option<Timestamp>,
    last_success_at: Option<Timestamp>,
    last_error_at: Option<Timestamp>,
    last_error_class: Option<String>,
    last_error_status: Option<u16>,
}

impl Default for LlmWorkloadState {
    fn default() -> Self {
        Self {
            calls: 0,
            successes: 0,
            errors: 0,
            latency_buckets: [0; LLM_WORKLOAD_BUCKETS],
            latency_total_us: 0,
            latency_max_us: 0,
            estimated_input_tokens: 0,
            estimated_output_tokens: 0,
            reported_input_tokens: 0,
            reported_output_tokens: 0,
            usage_reported_calls: 0,
            status: ProviderHealthStatus::Unknown,
            last_call_at: None,
            last_success_at: None,
            last_error_at: None,
            last_error_class: None,
            last_error_status: None,
        }
    }
}

impl LlmWorkloadState {
    fn snapshot(&self, workload: LlmWorkload) -> LlmWorkloadHealthSnapshot {
        LlmWorkloadHealthSnapshot {
            workload,
            calls: self.calls,
            successes: self.successes,
            errors: self.errors,
            latency_total_us: self.latency_total_us,
            latency_max_us: self.latency_max_us,
            latency_p50_upper_us: percentile_upper_bound(&self.latency_buckets, self.calls, 50),
            latency_p95_upper_us: percentile_upper_bound(&self.latency_buckets, self.calls, 95),
            estimated_input_tokens: self.estimated_input_tokens,
            estimated_output_tokens: self.estimated_output_tokens,
            reported_input_tokens: self.reported_input_tokens,
            reported_output_tokens: self.reported_output_tokens,
            usage_reported_calls: self.usage_reported_calls,
            status: self.status,
            last_call_at: self.last_call_at,
            last_success_at: self.last_success_at,
            last_error_at: self.last_error_at,
            last_error_class: self.last_error_class.clone(),
            last_error_status: self.last_error_status,
        }
    }
}

fn percentile_upper_bound(
    buckets: &[u64; LLM_WORKLOAD_BUCKETS],
    count: u64,
    percentile: u64,
) -> Option<u64> {
    if count == 0 {
        return None;
    }
    let target = count.saturating_mul(percentile).div_ceil(100).max(1);
    let mut cumulative = 0_u64;
    for (bound, bucket) in ai_memory_core::LATENCY_BUCKET_BOUNDS_US.iter().zip(buckets) {
        cumulative = cumulative.saturating_add(*bucket);
        if cumulative >= target {
            return Some(*bound);
        }
    }
    None
}

fn estimate_request_tokens(request: &ChatRequest) -> u64 {
    let chars = request
        .system
        .as_deref()
        .map_or(0, |value| value.chars().count())
        + request
            .messages
            .iter()
            .map(|message| message.content.chars().count())
            .sum::<usize>();
    u64::try_from(chars.div_ceil(4)).unwrap_or(u64::MAX)
}

fn estimate_text_tokens(text: &str) -> u64 {
    u64::try_from(text.chars().count().div_ceil(4)).unwrap_or(u64::MAX)
}

fn estimate_json_tokens(value: &serde_json::Value) -> u64 {
    estimate_text_tokens(&value.to_string())
}

fn estimate_structured_input_tokens(request: &ChatRequest, schema: &serde_json::Value) -> u64 {
    estimate_request_tokens(request).saturating_add(estimate_json_tokens(schema))
}

struct HealthRecordingLlmProvider {
    inner: Arc<dyn LlmProvider>,
    health: ProviderRoleHealth,
    workloads: LlmWorkloadHealth,
}

#[async_trait]
impl LlmProvider for HealthRecordingLlmProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let estimated_input = estimate_request_tokens(&request);
        let started = Instant::now();
        let result = self.inner.complete(request).await;
        self.health.record_result(&result);
        self.workloads.record(
            LlmWorkload::Unspecified,
            started.elapsed(),
            estimated_input,
            &result,
            |response| estimate_text_tokens(&response.text),
            |response| response.usage,
        );
        result
    }

    // The operation-id variants must be forwarded, not left to the trait
    // defaults: those call `self.complete*`, which drops the caller's id, so
    // the inner provider would mint a fresh one per call and every retry of
    // one logical operation would look unrelated on the wire.
    async fn complete_with_operation_id(
        &self,
        request: ChatRequest,
        operation_id: LlmOperationId,
    ) -> LlmResult<ChatResponse> {
        let estimated_input = estimate_request_tokens(&request);
        let started = Instant::now();
        let result = self
            .inner
            .complete_with_operation_id(request, operation_id)
            .await;
        self.health.record_result(&result);
        self.workloads.record(
            operation_id.workload(),
            started.elapsed(),
            estimated_input,
            &result,
            |response| estimate_text_tokens(&response.text),
            |response| response.usage,
        );
        result
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        let estimated_input = estimate_structured_input_tokens(&request, &schema);
        let started = Instant::now();
        let result = self.inner.complete_structured_raw(request, schema).await;
        self.health.record_result(&result);
        self.workloads.record(
            LlmWorkload::Unspecified,
            started.elapsed(),
            estimated_input,
            &result,
            estimate_json_tokens,
            |_| None,
        );
        result
    }

    async fn complete_structured_raw_with_operation_id(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
        operation_id: LlmOperationId,
    ) -> LlmResult<serde_json::Value> {
        let estimated_input = estimate_structured_input_tokens(&request, &schema);
        let started = Instant::now();
        let result = self
            .inner
            .complete_structured_raw_with_operation_id(request, schema, operation_id)
            .await;
        self.health.record_result(&result);
        self.workloads.record(
            operation_id.workload(),
            started.elapsed(),
            estimated_input,
            &result,
            estimate_json_tokens,
            |_| None,
        );
        result
    }

    fn candidate_health(&self) -> Vec<CandidateHealth> {
        self.inner.candidate_health()
    }
}

struct HealthRecordingEmbedder {
    inner: Arc<dyn Embedder>,
    health: ProviderRoleHealth,
}

#[async_trait]
impl Embedder for HealthRecordingEmbedder {
    fn provider(&self) -> &'static str {
        self.inner.provider()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn model_identity(&self) -> String {
        // Not the default (`self.model().to_string()`): every configured
        // embedder is wrapped in this type before being handed to the rest
        // of the server (`serve.rs`'s `wrap_embedder`), so without this
        // override every caller of `model_identity` — the refuse-on-mismatch
        // check, backfill, retrieval, cleanup — would silently see the
        // wire model name instead of the inner embedder's actual document-
        // prefix-aware identity.
        self.inner.model_identity()
    }

    fn dim(&self) -> u32 {
        self.inner.dim()
    }

    async fn embed(&self, text: &str) -> LlmResult<Vec<f32>> {
        let result = self.inner.embed(text).await;
        self.health.record_result(&result);
        result
    }

    async fn embed_document(&self, text: &str) -> LlmResult<Vec<f32>> {
        let result = self.inner.embed_document(text).await;
        self.health.record_result(&result);
        result
    }

    async fn embed_query(&self, text: &str) -> LlmResult<Vec<f32>> {
        let result = self.inner.embed_query(text).await;
        self.health.record_result(&result);
        result
    }
}

fn error_status_and_message(err: &LlmError) -> (Option<u16>, String) {
    match err {
        LlmError::Http(e) => (
            e.status().map(|status| status.as_u16()),
            truncate_error_message(&e.to_string()),
        ),
        LlmError::Provider { status, body, .. } => (Some(*status), truncate_error_message(body)),
        _ => (None, truncate_error_message(&err.to_string())),
    }
}

fn truncate_error_message(message: &str) -> String {
    let mut chars = message.chars();
    let truncated: String = chars.by_ref().take(MAX_ERROR_MESSAGE_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeLlm {
        fail: bool,
    }

    #[async_trait]
    impl LlmProvider for FakeLlm {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn model(&self) -> &str {
            "fake-model"
        }

        async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            if self.fail {
                return Err(LlmError::Provider {
                    status: 401,
                    body: "bad token".to_string(),
                    retry_after: None,
                });
            }
            Ok(ChatResponse {
                text: "pong".to_string(),
                usage: Some(Usage {
                    input_tokens: 2,
                    output_tokens: 1,
                }),
                model: "fake-model".to_string(),
            })
        }

        async fn complete_structured_raw(
            &self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> LlmResult<serde_json::Value> {
            self.complete(ChatRequest::user_prompt("ping"))
                .await
                .map(|_| serde_json::json!({ "ok": true }))
        }
    }

    struct TaskAwareEmbedder;

    #[async_trait]
    impl Embedder for TaskAwareEmbedder {
        fn provider(&self) -> &'static str {
            "google"
        }

        fn model(&self) -> &str {
            "gemini-embedding-001"
        }

        fn dim(&self) -> u32 {
            2
        }

        async fn embed(&self, _text: &str) -> LlmResult<Vec<f32>> {
            Ok(vec![0.0, 0.0])
        }

        async fn embed_document(&self, _text: &str) -> LlmResult<Vec<f32>> {
            Ok(vec![1.0, 0.0])
        }

        async fn embed_query(&self, _text: &str) -> LlmResult<Vec<f32>> {
            Ok(vec![0.0, 1.0])
        }
    }

    /// Records the operation id each operation-aware call receives.
    #[derive(Default)]
    struct OperationIdLlm {
        seen: Mutex<Vec<LlmOperationId>>,
    }

    #[async_trait]
    impl LlmProvider for OperationIdLlm {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn model(&self) -> &str {
            "fake-model"
        }

        async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                text: "pong".to_string(),
                usage: Some(Usage {
                    input_tokens: 2,
                    output_tokens: 1,
                }),
                model: "fake-model".to_string(),
            })
        }

        async fn complete_with_operation_id(
            &self,
            request: ChatRequest,
            operation_id: LlmOperationId,
        ) -> LlmResult<ChatResponse> {
            self.seen.lock().unwrap().push(operation_id);
            self.complete(request).await
        }

        async fn complete_structured_raw(
            &self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> LlmResult<serde_json::Value> {
            Ok(serde_json::json!({ "ok": true }))
        }

        async fn complete_structured_raw_with_operation_id(
            &self,
            request: ChatRequest,
            schema: serde_json::Value,
            operation_id: LlmOperationId,
        ) -> LlmResult<serde_json::Value> {
            self.seen.lock().unwrap().push(operation_id);
            self.complete_structured_raw(request, schema).await
        }
    }

    #[tokio::test]
    async fn llm_wrapper_forwards_the_callers_operation_id() {
        let health = ProviderHealth::default();
        let inner = Arc::new(OperationIdLlm::default());
        let llm = health.wrap_llm_provider(inner.clone(), "openai", "gpt-test", None);
        let id = LlmOperationId::for_workload(LlmWorkload::Consolidation);

        llm.complete_with_operation_id(ChatRequest::user_prompt("ping"), id)
            .await
            .unwrap();
        llm.complete_structured_raw_with_operation_id(
            ChatRequest::user_prompt("ping"),
            serde_json::json!({}),
            id,
        )
        .await
        .unwrap();

        assert_eq!(*inner.seen.lock().unwrap(), vec![id, id]);
        let snapshot = health.snapshot();
        assert_eq!(snapshot.llm.status, ProviderHealthStatus::Ok);
        let consolidation = snapshot
            .llm_workloads
            .iter()
            .find(|row| row.workload == LlmWorkload::Consolidation)
            .unwrap();
        assert_eq!(
            (
                consolidation.calls,
                consolidation.successes,
                consolidation.errors,
                consolidation.usage_reported_calls,
                consolidation.reported_input_tokens,
                consolidation.reported_output_tokens,
            ),
            (2, 2, 0, 1, 2, 1)
        );
        assert!(consolidation.estimated_input_tokens > 0);
        assert!(consolidation.estimated_output_tokens > 0);
        assert!(consolidation.latency_p50_upper_us.is_some());
        assert!(
            !id.to_string().contains(LlmWorkload::Consolidation.as_str()),
            "the workload label must not change the provider wire id"
        );
    }

    #[tokio::test]
    async fn llm_wrapper_records_unknown_success_and_error() {
        let health = ProviderHealth::default();
        let retry_hint =
            "ai-memory llm-test --provider anthropic-oauth --model claude-sonnet-4-6 --prompt ping"
                .to_string();
        let llm = health.wrap_llm_provider(
            Arc::new(FakeLlm { fail: false }),
            "anthropic-oauth",
            "claude-sonnet-4-6",
            Some(retry_hint.clone()),
        );

        let before = health.snapshot().llm;
        assert_eq!(before.status, ProviderHealthStatus::Unknown);
        assert_eq!(before.provider.as_deref(), Some("anthropic-oauth"));
        assert_eq!(before.model.as_deref(), Some("claude-sonnet-4-6"));
        assert!(before.retry_hint.is_none());

        llm.complete(ChatRequest::user_prompt("ping"))
            .await
            .unwrap();
        let snapshot = health.snapshot();
        let after_success = snapshot.llm;
        assert_eq!(after_success.status, ProviderHealthStatus::Ok);
        assert!(after_success.last_call_at.is_some());
        assert!(after_success.last_success_at.is_some());
        assert!(after_success.last_error_message.is_none());
        let unspecified = snapshot
            .llm_workloads
            .iter()
            .find(|row| row.workload == LlmWorkload::Unspecified)
            .unwrap();
        assert_eq!(unspecified.calls, 1);
        assert_eq!(unspecified.usage_reported_calls, 1);
        assert_eq!(unspecified.reported_input_tokens, 2);
        assert_eq!(unspecified.reported_output_tokens, 1);

        let llm = health.wrap_llm_provider(
            Arc::new(FakeLlm { fail: true }),
            "anthropic-oauth",
            "claude-sonnet-4-6",
            Some(retry_hint.clone()),
        );
        let err = llm.complete(ChatRequest::user_prompt("ping")).await;
        assert!(err.is_err());
        let after_error = health.snapshot().llm;
        assert_eq!(after_error.status, ProviderHealthStatus::Error);
        assert_eq!(after_error.last_error_status, Some(401));
        assert_eq!(after_error.last_error_message.as_deref(), Some("bad token"));
        assert_eq!(after_error.retry_hint.as_deref(), Some(retry_hint.as_str()));
    }

    #[test]
    fn workload_snapshot_is_bounded_redacted_and_percentiled() {
        let workloads = LlmWorkloadHealth::default();
        let request = ChatRequest::user_prompt("do not expose this private prompt");
        let response = Ok(ChatResponse {
            text: "private provider output".into(),
            usage: Some(Usage {
                input_tokens: 11,
                output_tokens: 3,
            }),
            model: "fake".into(),
        });
        workloads.record(
            LlmWorkload::Rerank,
            Duration::from_millis(40),
            estimate_request_tokens(&request),
            &response,
            |output| estimate_text_tokens(&output.text),
            |output| output.usage,
        );
        let failure: LlmResult<ChatResponse> = Err(LlmError::Provider {
            status: 429,
            body: "secret upstream body".into(),
            retry_after: None,
        });
        workloads.record(
            LlmWorkload::Rerank,
            Duration::from_secs(12),
            estimate_request_tokens(&request),
            &failure,
            |output| estimate_text_tokens(&output.text),
            |output| output.usage,
        );
        workloads.record(
            LlmWorkload::Rerank,
            Duration::from_millis(20),
            estimate_request_tokens(&request),
            &response,
            |output| estimate_text_tokens(&output.text),
            |output| output.usage,
        );

        let row = workloads
            .snapshot()
            .into_iter()
            .find(|row| row.workload == LlmWorkload::Rerank)
            .unwrap();
        assert_eq!((row.calls, row.successes, row.errors), (3, 2, 1));
        assert_eq!(
            (row.reported_input_tokens, row.reported_output_tokens),
            (22, 6)
        );
        assert_eq!(row.usage_reported_calls, 2);
        assert_eq!(row.status, ProviderHealthStatus::Ok);
        assert!(row.last_success_at.is_some());
        assert_eq!(row.latency_p50_upper_us, Some(50_000));
        assert_eq!(row.latency_p95_upper_us, None, "p95 is above 10 seconds");
        assert!(
            row.last_error_at.is_some(),
            "a later success must not erase the last error evidence"
        );
        assert_eq!(row.last_error_class.as_deref(), Some("provider"));
        assert_eq!(row.last_error_status, Some(429));
        let json = serde_json::to_string(&row).unwrap();
        for secret in ["private prompt", "provider output", "upstream body"] {
            assert!(!json.contains(secret), "telemetry leaked {secret}: {json}");
        }
    }

    #[test]
    fn structured_input_estimate_includes_the_schema() {
        let request = ChatRequest::user_prompt("x");
        let schema = serde_json::json!({
            "type": "object",
            "description": "s".repeat(400),
        });
        assert!(
            estimate_structured_input_tokens(&request, &schema)
                > estimate_request_tokens(&request) + 90
        );
    }

    #[tokio::test]
    async fn embedder_wrapper_records_and_preserves_task_specific_methods() {
        let health = ProviderHealth::default();
        let embedder = health.wrap_embedder(
            Arc::new(TaskAwareEmbedder),
            "google",
            "gemini-embedding-001",
            2,
        );

        let before = health.snapshot().embedding;
        assert_eq!(before.status, ProviderHealthStatus::Unknown);
        assert_eq!(before.dim, Some(2));

        assert_eq!(
            embedder.embed_document("doc").await.unwrap(),
            vec![1.0, 0.0]
        );
        assert_eq!(embedder.embed_query("query").await.unwrap(), vec![0.0, 1.0]);

        let after = health.snapshot().embedding;
        assert_eq!(after.status, ProviderHealthStatus::Ok);
        assert!(after.last_call_at.is_some());
    }

    /// Every configured embedder is wrapped in `HealthRecordingEmbedder`
    /// before reaching the rest of the server (`serve.rs`'s
    /// `wrap_embedder`), so a caller of `model_identity` — the
    /// refuse-on-mismatch check, backfill, retrieval, cleanup — only ever
    /// sees the wrapper, never the inner embedder directly. Without the
    /// wrapper's own `model_identity` override, the default trait impl
    /// (`self.model().to_string()`) would silently discard a document
    /// prefix's identity fingerprint. `TaskAwareEmbedder` doesn't
    /// distinguish this (it never overrides `model_identity` either, so a
    /// missing wrapper override would coincidentally still pass against
    /// it); a real `OpenAiCompatEmbedder` with a document prefix set does.
    #[test]
    fn embedder_wrapper_forwards_the_inner_model_identity_override() {
        let inner =
            crate::OpenAiCompatEmbedder::new("http://localhost:9/v1", None, "nomic-embed-text", 8)
                .expect("embedder builds")
                .with_prefixes("query: ", "passage: ");
        let inner_identity = inner.model_identity();
        assert_ne!(
            inner_identity,
            inner.model(),
            "the fixture must actually have a distinct identity, or this test proves nothing"
        );

        let health = ProviderHealth::default();
        let wrapped = health.wrap_embedder(Arc::new(inner), "openai-compat", "nomic-embed-text", 8);
        assert_eq!(wrapped.model_identity(), inner_identity);
    }

    #[test]
    fn provider_health_status_wire_labels() {
        for (status, label) in [
            (ProviderHealthStatus::Disabled, "disabled"),
            (ProviderHealthStatus::Unknown, "unknown"),
            (ProviderHealthStatus::Ok, "ok"),
            (ProviderHealthStatus::Error, "error"),
        ] {
            assert_eq!(status.as_str(), label);
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::json!(label)
            );
            assert_eq!(
                serde_json::from_value::<ProviderHealthStatus>(serde_json::json!(label)).unwrap(),
                status
            );
        }
    }
}
