//! Opt-in shared admission for chat-provider calls.
//!
//! [`AdmittedLlmProvider`] serialises every call that goes through one
//! instance (all clones of its `Arc`) behind a fair FIFO gate, so a
//! long-running job that re-enters after each call cannot starve a short
//! call that arrived while it was executing. The gate coordinates callers
//! inside this process only: it cannot preempt a running call and does not
//! see other processes sharing the same backend.
//!
//! Waiting is bounded in two ways, and both refuse with
//! [`LlmError::CapacityBusy`] — a local back-pressure signal, not a
//! provider fault:
//! - at most [`MAX_WAITING`] calls wait behind the active one (the slot is
//!   held for the whole wait + execution, and released if the caller's
//!   future is dropped);
//! - a waiting call gives up after [`WAIT_TIMEOUT`]. Once admitted a call
//!   runs under the inner provider's own deadline only.
//!
//! [`LlmWorkload::Rerank`] calls never wait: they run immediately when the
//! gate is idle and otherwise fail fast, so search degrades to its
//! pre-rerank order instead of queueing behind a long call.
//!
//! Wiring: install this wrapper *outside* the health-recording wrapper.
//! `CapacityBusy` is raised before the inner provider is touched, so an
//! outer placement keeps local saturation out of provider health.

use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::error::{LlmError, LlmResult};
use crate::health::CandidateHealth;
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse, LlmOperationId, LlmWorkload};

/// Calls allowed to wait behind the single active call.
const MAX_WAITING: usize = 32;
/// Longest a call waits for admission before giving up.
const WAIT_TIMEOUT: Duration = Duration::from_secs(300);
/// Label carried by the [`LlmError::CapacityBusy`] this module raises.
const BUSY: &str = "llm admission";

tokio::task_local! {
    /// Set while a call runs inside the gate, so a provider that re-enters
    /// an admitted provider (directly or through a nested wrapper) does not
    /// queue behind itself.
    static ADMITTED: ();
}

/// Admission wait and provider execution of one captured call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallTiming {
    /// Time queued behind other calls for the gate.
    pub wait: Duration,
    /// Time the provider itself ran once admitted.
    pub exec: Duration,
}

tokio::task_local! {
    /// Collector of the innermost [`capture_call_timing`] scope; `None`
    /// until an admission gate records into it.
    static CAPTURE: Cell<Option<CallTiming>>;
}

/// Run `call` and report where its time went. An [`AdmittedLlmProvider`]
/// reached by `call` records its actual queue wait and execution (summed
/// over every gate pass); a provider with no gate records nothing, so the
/// whole elapsed time counts as execution. Scoped to the awaiting task, so
/// concurrent captures never mix; work spawned onto another task is not seen.
pub async fn capture_call_timing<F: Future>(call: F) -> (F::Output, CallTiming) {
    let started = Instant::now();
    let (output, recorded) = CAPTURE
        .scope(Cell::new(None), async {
            let output = call.await;
            (output, CAPTURE.with(Cell::take))
        })
        .await;
    let timing = recorded.unwrap_or(CallTiming {
        wait: Duration::ZERO,
        exec: started.elapsed(),
    });
    (output, timing)
}

fn record(wait: Duration, exec: Duration) {
    let _ = CAPTURE.try_with(|cell| {
        let seen = cell.take().unwrap_or_default();
        cell.set(Some(CallTiming {
            wait: seen.wait.saturating_add(wait),
            exec: seen.exec.saturating_add(exec),
        }));
    });
}

/// Fair FIFO admission wrapper for a chat [`LlmProvider`].
pub struct AdmittedLlmProvider {
    inner: Arc<dyn LlmProvider>,
    /// One permit: the executing call. Tokio semaphores are FIFO-fair.
    active: Arc<Semaphore>,
    /// `1 + waiting` permits, held from entry until the call finishes.
    slots: Arc<Semaphore>,
    wait_timeout: Duration,
}

impl AdmittedLlmProvider {
    /// Wrap `inner` with the shared admission gate.
    #[must_use]
    pub fn new(inner: Arc<dyn LlmProvider>) -> Self {
        Self::with_limits(inner, MAX_WAITING, WAIT_TIMEOUT)
    }

    fn with_limits(inner: Arc<dyn LlmProvider>, waiting: usize, wait_timeout: Duration) -> Self {
        Self {
            inner,
            active: Arc::new(Semaphore::new(1)),
            slots: Arc::new(Semaphore::new(waiting + 1)),
            wait_timeout,
        }
    }

    /// Run `call` once admitted. `call` is not polled before admission.
    async fn admit<T, F>(&self, workload: LlmWorkload, call: F) -> LlmResult<T>
    where
        F: Future<Output = LlmResult<T>> + Send,
    {
        if ADMITTED.try_with(|()| ()).is_ok() {
            return call.await;
        }
        let started = Instant::now();
        let (_slot, _active) = match self.acquire(workload, started).await {
            Ok(held) => held,
            Err(e) => {
                record(started.elapsed(), Duration::ZERO);
                return Err(e);
            }
        };
        let waited = started.elapsed();
        let executing = Instant::now();
        let result = ADMITTED.scope((), call).await;
        let exec = executing.elapsed();
        record(waited, exec);
        debug!(
            workload = workload.as_str(),
            provider = self.inner.name(),
            model = self.inner.model(),
            wait_ms = waited.as_millis(),
            exec_ms = exec.as_millis(),
            ok = result.is_ok(),
            "llm admission call finished"
        );
        result
    }

    /// Take the slot and the execution permit; `Err` is local back-pressure.
    /// The slot is `None` for rerank: it never waits, so it needs no slot.
    async fn acquire(
        &self,
        workload: LlmWorkload,
        started: Instant,
    ) -> LlmResult<(Option<OwnedSemaphorePermit>, OwnedSemaphorePermit)> {
        if workload == LlmWorkload::Rerank {
            let active = Arc::clone(&self.active).try_acquire_owned().map_err(|_| {
                debug!(
                    workload = workload.as_str(),
                    "llm admission busy for rerank"
                );
                LlmError::CapacityBusy(BUSY)
            })?;
            return Ok((None, active));
        }
        let slot = Arc::clone(&self.slots).try_acquire_owned().map_err(|_| {
            warn!(
                workload = workload.as_str(),
                "llm admission queue full; refusing call"
            );
            LlmError::CapacityBusy(BUSY)
        })?;
        let active =
            tokio::time::timeout(self.wait_timeout, Arc::clone(&self.active).acquire_owned())
                .await
                .ok()
                .and_then(Result::ok)
                .ok_or_else(|| {
                    warn!(
                        workload = workload.as_str(),
                        waited_ms = started.elapsed().as_millis(),
                        "llm admission wait timed out"
                    );
                    LlmError::CapacityBusy(BUSY)
                })?;
        Ok((Some(slot), active))
    }
}

#[async_trait]
impl LlmProvider for AdmittedLlmProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn model(&self) -> &str {
        self.inner.model()
    }

    fn cache_identity(&self) -> String {
        self.inner.cache_identity()
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        self.admit(LlmWorkload::Unspecified, self.inner.complete(request))
            .await
    }

    async fn complete_with_operation_id(
        &self,
        request: ChatRequest,
        operation_id: LlmOperationId,
    ) -> LlmResult<ChatResponse> {
        self.admit(
            operation_id.workload(),
            self.inner.complete_with_operation_id(request, operation_id),
        )
        .await
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        self.admit(
            LlmWorkload::Unspecified,
            self.inner.complete_structured_raw(request, schema),
        )
        .await
    }

    async fn complete_structured_raw_with_operation_id(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
        operation_id: LlmOperationId,
    ) -> LlmResult<serde_json::Value> {
        self.admit(
            operation_id.workload(),
            self.inner
                .complete_structured_raw_with_operation_id(request, schema, operation_id),
        )
        .await
    }

    fn candidate_health(&self) -> Vec<CandidateHealth> {
        self.inner.candidate_health()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::OnceLock;

    use tokio::sync::mpsc;
    use tokio::task::JoinHandle;

    use super::*;

    /// Provider that announces each call on `entered` (labelled by the
    /// request text) and then blocks until `gate` has a permit.
    struct Gated {
        entered: mpsc::UnboundedSender<String>,
        gate: Arc<Semaphore>,
        reenter: OnceLock<Arc<AdmittedLlmProvider>>,
    }

    impl Gated {
        async fn run(&self, request: &ChatRequest) {
            let label = request.messages[0].content.clone();
            self.entered.send(label.clone()).unwrap();
            if label == "reenter" {
                // A call that re-enters its own admitted provider must not
                // queue behind itself.
                let outer = self.reenter.get().unwrap();
                outer
                    .complete(ChatRequest::user_prompt("inner"))
                    .await
                    .unwrap();
            }
            self.gate.acquire().await.unwrap().forget();
        }
    }

    #[async_trait]
    impl LlmProvider for Gated {
        fn name(&self) -> &'static str {
            "gated"
        }

        fn model(&self) -> &str {
            "gated-model"
        }

        async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
            self.run(&request).await;
            Ok(response())
        }

        async fn complete_structured_raw(
            &self,
            request: ChatRequest,
            _schema: serde_json::Value,
        ) -> LlmResult<serde_json::Value> {
            self.run(&request).await;
            Ok(serde_json::json!({}))
        }
    }

    fn response() -> ChatResponse {
        ChatResponse {
            text: "ok".into(),
            usage: None,
            model: "gated-model".into(),
        }
    }

    struct Stage {
        gated: Arc<Gated>,
        admitted: Arc<AdmittedLlmProvider>,
        gate: Arc<Semaphore>,
        entered: mpsc::UnboundedReceiver<String>,
    }

    impl Stage {
        fn new(waiting: usize, wait_timeout: Duration) -> Self {
            let (entered_tx, entered) = mpsc::unbounded_channel();
            let gate = Arc::new(Semaphore::new(0));
            let gated = Arc::new(Gated {
                entered: entered_tx,
                gate: Arc::clone(&gate),
                reenter: OnceLock::new(),
            });
            let admitted = Arc::new(AdmittedLlmProvider::with_limits(
                Arc::clone(&gated) as Arc<dyn LlmProvider>,
                waiting,
                wait_timeout,
            ));
            Self {
                gated,
                admitted,
                gate,
                entered,
            }
        }

        fn call(&self, label: &'static str) -> JoinHandle<LlmResult<ChatResponse>> {
            let admitted = Arc::clone(&self.admitted);
            tokio::spawn(async move { admitted.complete(ChatRequest::user_prompt(label)).await })
        }

        fn release(&self, n: usize) {
            self.gate.add_permits(n);
        }

        async fn next_entered(&mut self) -> String {
            self.entered.recv().await.unwrap()
        }
    }

    impl Default for Stage {
        fn default() -> Self {
            Self::new(MAX_WAITING, WAIT_TIMEOUT)
        }
    }

    /// Let every spawned task run until it parks. Paused time auto-advances
    /// only when all tasks are idle, so this also orders queue entry.
    async fn settle() {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    fn is_busy<T: std::fmt::Debug>(result: &LlmResult<T>) -> bool {
        matches!(result, Err(LlmError::CapacityBusy(BUSY)))
    }

    #[tokio::test(start_paused = true)]
    async fn short_call_runs_between_two_calls_of_a_long_job() {
        let mut stage = Stage::default();
        // A long job: two sequential calls, re-contending for each.
        let admitted = Arc::clone(&stage.admitted);
        let job = tokio::spawn(async move {
            admitted
                .complete(ChatRequest::user_prompt("long-1"))
                .await
                .unwrap();
            admitted
                .complete(ChatRequest::user_prompt("long-2"))
                .await
                .unwrap();
        });
        assert_eq!(stage.next_entered().await, "long-1");
        let short = stage.call("short");
        settle().await;

        stage.release(1);
        // The short call queued while long-1 ran, so it goes before long-2.
        assert_eq!(stage.next_entered().await, "short");
        stage.release(1);
        assert_eq!(stage.next_entered().await, "long-2");
        stage.release(1);
        short.await.unwrap().unwrap();
        job.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_calls_run_in_arrival_order() {
        let mut stage = Stage::default();
        let labels = ["a", "b", "c", "d"];
        let handles: Vec<_> = labels.into_iter().map(|label| stage.call(label)).collect();
        for label in labels {
            // Spawn order equals queue order because each task parks on the
            // gate before the next one is polled on this single-thread runtime.
            assert_eq!(stage.next_entered().await, label);
            stage.release(1);
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn saturation_refuses_and_cancellation_frees_a_slot() {
        // One active + 2 waiting = 3 slots.
        let mut stage = Stage::new(2, WAIT_TIMEOUT);
        let active = stage.call("active");
        assert_eq!(stage.next_entered().await, "active");
        let doomed = stage.call("doomed");
        let kept = stage.call("kept");
        settle().await;

        let refused = stage
            .admitted
            .complete(ChatRequest::user_prompt("overflow"))
            .await;
        assert!(is_busy(&refused), "{refused:?}");

        // Dropping a waiting future releases its slot (and its queue place).
        doomed.abort();
        let _ = doomed.await;
        let late = stage.call("late");
        settle().await;

        stage.release(1);
        active.await.unwrap().unwrap();
        assert_eq!(stage.next_entered().await, "kept");
        stage.release(1);
        kept.await.unwrap().unwrap();
        assert_eq!(stage.next_entered().await, "late");
        stage.release(1);
        late.await.unwrap().unwrap();
        // The aborted call never reached the provider.
        assert!(stage.entered.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn wait_times_out_as_capacity_busy_but_active_call_has_no_deadline() {
        let mut stage = Stage::default();
        let active = stage.call("active");
        assert_eq!(stage.next_entered().await, "active");

        let started = Instant::now();
        let waiter = stage
            .admitted
            .complete(ChatRequest::user_prompt("waiter"))
            .await;
        assert!(is_busy(&waiter), "{waiter:?}");
        assert!(started.elapsed() >= WAIT_TIMEOUT);
        // The refused waiter must not have reached the provider.
        assert!(stage.entered.try_recv().is_err());

        // The active call is far past the wait timeout and still runs.
        tokio::time::sleep(Duration::from_secs(3_600)).await;
        assert!(!active.is_finished());
        stage.release(1);
        active.await.unwrap().unwrap();

        // The timed-out waiter's slot is free again.
        stage.release(1);
        stage
            .admitted
            .complete(ChatRequest::user_prompt("after"))
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn rerank_runs_when_idle_and_fails_fast_when_busy_or_queued() {
        let mut stage = Stage::default();
        let schema = serde_json::json!({});
        let rerank = |stage: &Stage| {
            let admitted = Arc::clone(&stage.admitted);
            let schema = schema.clone();
            async move {
                admitted
                    .complete_structured_raw_with_operation_id(
                        ChatRequest::user_prompt("rerank"),
                        schema,
                        LlmOperationId::for_workload(LlmWorkload::Rerank),
                    )
                    .await
            }
        };

        // Idle: runs immediately.
        stage.release(1);
        rerank(&stage).await.unwrap();
        assert_eq!(stage.next_entered().await, "rerank");

        // A long call executing: rerank refuses without waiting or reaching
        // the provider.
        let long = stage.call("long");
        assert_eq!(stage.next_entered().await, "long");
        let started = Instant::now();
        let refused = rerank(&stage).await;
        assert!(is_busy(&refused), "{refused:?}");
        assert_eq!(started.elapsed(), Duration::ZERO);

        // Long call finishing while another call is queued: the queued call
        // owns the gate, so rerank still does not jump ahead of it.
        let queued = stage.call("queued");
        settle().await;
        stage.release(1);
        long.await.unwrap().unwrap();
        assert_eq!(stage.next_entered().await, "queued");
        let refused = rerank(&stage).await;
        assert!(is_busy(&refused), "{refused:?}");
        stage.release(1);
        queued.await.unwrap().unwrap();
        assert!(stage.entered.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn reentrant_call_does_not_queue_behind_itself() {
        let stage = Stage::default();
        stage.gated.reenter.set(Arc::clone(&stage.admitted)).ok();
        stage.release(2);
        stage
            .admitted
            .complete(ChatRequest::user_prompt("reenter"))
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn capture_separates_queue_wait_from_execution() {
        let mut stage = Stage::default();
        let active = stage.call("active");
        assert_eq!(stage.next_entered().await, "active");
        let admitted = Arc::clone(&stage.admitted);
        let queued = tokio::spawn(capture_call_timing(async move {
            admitted.complete(ChatRequest::user_prompt("queued")).await
        }));
        settle().await;

        tokio::time::sleep(Duration::from_secs(5)).await;
        stage.release(1);
        active.await.unwrap().unwrap();
        assert_eq!(stage.next_entered().await, "queued");
        tokio::time::sleep(Duration::from_secs(2)).await;
        stage.release(1);

        let (result, timing) = queued.await.unwrap();
        result.unwrap();
        assert!(timing.wait >= Duration::from_secs(5) && timing.wait < Duration::from_secs(6));
        assert!(timing.exec >= Duration::from_secs(2) && timing.exec < Duration::from_secs(3));

        // A provider with no gate: everything counts as execution.
        let (_, ungated) = capture_call_timing(tokio::time::sleep(Duration::from_secs(3))).await;
        assert_eq!(ungated.wait, Duration::ZERO);
        assert_eq!(ungated.exec, Duration::from_secs(3));
    }
}
