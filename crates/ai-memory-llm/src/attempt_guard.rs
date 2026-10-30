//! Task-local guard consulted before every actual generation send.
//!
//! One `LlmProvider` invocation can send more than one generation request:
//! the openai-compat strict→tolerant re-send, the Codex 401 re-POST and every
//! fallback-chain candidate. A caller that budgets generation attempts
//! installs a [`GenerationAttemptGuard`] around the invocation with
//! [`with_generation_attempt_guard`]; each provider then calls
//! [`before_generation_send`] immediately before its terminal send and
//! aborts with the guard's error when it refuses.
//!
//! The caller already durably reserved one attempt for the invocation, so the
//! first send in a scope reports `prepaid = true` and every later one
//! `prepaid = false` (the guard must reserve it itself). Outside a scope the
//! hook is a no-op, so providers behave exactly as before.
//!
//! A refusal should be [`crate::LlmError::CapacityBusy`]: it is local back-pressure,
//! so health recording and the fallback chain neither mark the provider failed
//! nor advance to another candidate.
//!
//! Scoped to the awaiting task like [`crate::capture_call_timing`]: work
//! spawned onto another task is not seen. Custom providers that send
//! generation requests themselves call [`before_generation_send`] too.

use std::cell::Cell;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::LlmResult;

/// Decides whether one more generation request may leave the process.
#[async_trait]
pub trait GenerationAttemptGuard: Send + Sync {
    /// Called before each generation send. `prepaid` is `true` for the first
    /// send of the scope (already reserved by the caller) and `false` for
    /// every later one. An `Err` aborts that send.
    async fn before_send(&self, prepaid: bool) -> LlmResult<()>;
}

struct Scope {
    guard: Arc<dyn GenerationAttemptGuard>,
    first: Cell<bool>,
}

tokio::task_local! {
    static SCOPE: Scope;
}

/// Run `call` with `guard` consulted before every generation send it makes.
pub async fn with_generation_attempt_guard<F: Future>(
    guard: Arc<dyn GenerationAttemptGuard>,
    call: F,
) -> F::Output {
    SCOPE
        .scope(
            Scope {
                guard,
                first: Cell::new(true),
            },
            call,
        )
        .await
}

/// Consult the scoped guard before a generation send; no-op outside a scope.
/// Place it immediately before the terminal request, never before token
/// refreshes, metadata or embeddings.
pub async fn before_generation_send() -> LlmResult<()> {
    let Ok((guard, prepaid)) = SCOPE.try_with(|s| (Arc::clone(&s.guard), s.first.replace(false)))
    else {
        return Ok(());
    };
    guard.before_send(prepaid).await
}
