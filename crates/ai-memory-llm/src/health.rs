//! Passive provider-health recording wrappers.
//!
//! These wrappers never probe providers on their own. They only record
//! the result of calls the server was already going to make.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::embedding::Embedder;
use crate::error::{LlmError, LlmResult};
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse};

const MAX_ERROR_MESSAGE_CHARS: usize = 1024;

/// Process-scoped health recorder for the configured provider roles.
#[derive(Clone, Default)]
pub struct ProviderHealth {
    llm: ProviderRoleHealth,
    embedding: ProviderRoleHealth,
}

impl ProviderHealth {
    /// Return a serializable snapshot of the latest recorded state.
    #[must_use]
    pub fn snapshot(&self) -> ProviderHealthSnapshot {
        ProviderHealthSnapshot {
            llm: self.llm.snapshot(),
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
        Arc::new(HealthRecordingLlmProvider { inner, health })
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

/// Wire-format health snapshot for all provider roles.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderHealthSnapshot {
    /// LLM provider role.
    pub llm: ProviderRoleHealthSnapshot,
    /// Embedding provider role.
    pub embedding: ProviderRoleHealthSnapshot,
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

struct HealthRecordingLlmProvider {
    inner: Arc<dyn LlmProvider>,
    health: ProviderRoleHealth,
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
        let result = self.inner.complete(request).await;
        self.health.record_result(&result);
        result
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        let result = self.inner.complete_structured_raw(request, schema).await;
        self.health.record_result(&result);
        result
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
        LlmError::Provider { status, body } => (Some(*status), truncate_error_message(body)),
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
                });
            }
            Ok(ChatResponse {
                text: "pong".to_string(),
                usage: None,
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
        let after_success = health.snapshot().llm;
        assert_eq!(after_success.status, ProviderHealthStatus::Ok);
        assert!(after_success.last_call_at.is_some());
        assert!(after_success.last_success_at.is_some());
        assert!(after_success.last_error_message.is_none());

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
}
