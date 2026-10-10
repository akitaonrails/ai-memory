//! Process-lifetime counters for the hook ingestion pipeline.
//!
//! Lives in `core` rather than `hooks` because both the hook router (which
//! writes these) and the admin status route (which reads them) need the
//! type, and `ai-memory-mcp` deliberately does not depend on
//! `ai-memory-hooks` outside dev-dependencies. Putting it here keeps that
//! boundary intact instead of adding a production edge between them.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

/// Process-lifetime counters for the hook ingestion pipeline.
///
/// Deliberately a fixed set of scalars, never a map. #428 asked for
/// per-source visibility; keying anything on a client-supplied value would
/// make this unbounded state reachable from the network, which is the same
/// class of problem the bounded queue exists to prevent. What an operator
/// needs to answer — "are hooks arriving, is anything being shed, is the
/// writer keeping up" — does not require per-source breakdown.
///
/// Relaxed ordering throughout: these are diagnostics read by a human, and
/// the hot path must not pay for a fence. A counter observed a few events
/// stale is fine; a lock on `/hook` is not.
///
/// Content-free by construction, like the spool health snapshot: counts and one
/// timestamp, no paths, prompts, or tool payloads.
#[derive(Debug, Default)]
pub struct IngestMetrics {
    /// Events accepted for processing (202 with work queued).
    accepted: AtomicU64,
    /// Events accepted but deliberately not stored — capture policy or the
    /// subagent drop. Still a 202: the client must not retry these.
    dropped_by_policy: AtomicU64,
    /// Events accepted but not stored because the authenticated user holds no
    /// `write` access to the project the capture resolved to.
    ///
    /// Separate from `dropped_by_policy` on purpose. A policy drop is the
    /// operator's own configuration working as intended; this one means
    /// somebody's agent has been recording into a repository all day and none
    /// of it is landing. Those want different answers, so they must not share
    /// a counter.
    dropped_unauthorized: AtomicU64,
    /// Batch items accepted but not stored because they can never be: an
    /// event with no session id that is not a SessionStart. Acknowledged so
    /// the spool drops them instead of retrying them ahead of valid events.
    dropped_invalid: AtomicU64,
    /// Events shed because the global ingest semaphore had no permit (429).
    shed_saturated: AtomicU64,
    /// Events shed by the per-source rate limiter (429).
    shed_rate_limited: AtomicU64,
    /// Completed hook deliveries classified as `stored`.
    stored: AtomicU64,
    /// Completed hook deliveries classified as `replayed`.
    replayed: AtomicU64,
    /// Completed hook deliveries classified as `resumed`.
    resumed: AtomicU64,
    /// Completed hook deliveries classified as `ignored_end`.
    ignored_end: AtomicU64,
    /// Completed hook deliveries classified as `dropped_collision`.
    dropped_collision: AtomicU64,
    /// Completed hook deliveries classified as `failed`.
    failed: AtomicU64,
    /// Unix milliseconds of the last newly persisted observation or effect, or 0
    /// when none has since this process started.
    last_persisted_ms: AtomicU64,
}

impl IngestMetrics {
    /// One event admitted for processing.
    pub fn record_accepted(&self) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
    }
    /// One event accepted-but-dropped by capture policy or subagent rules.
    pub fn record_dropped_by_policy(&self) {
        self.dropped_by_policy.fetch_add(1, Ordering::Relaxed);
    }
    /// One capture dropped because its author may not write that repository.
    pub fn record_dropped_unauthorized(&self) {
        self.dropped_unauthorized.fetch_add(1, Ordering::Relaxed);
    }
    /// One batch item dropped because it can never be stored.
    pub fn record_dropped_invalid(&self) {
        self.dropped_invalid.fetch_add(1, Ordering::Relaxed);
    }
    /// One event shed because ingest capacity was exhausted.
    pub fn record_shed_saturated(&self) {
        self.shed_saturated.fetch_add(1, Ordering::Relaxed);
    }
    /// One event shed by the per-source rate limiter.
    pub fn record_shed_rate_limited(&self) {
        self.shed_rate_limited.fetch_add(1, Ordering::Relaxed);
    }
    /// Record one `stored` delivery.
    pub fn record_stored(&self) {
        self.stored.fetch_add(1, Ordering::Relaxed);
    }
    /// Record one `replayed` delivery.
    pub fn record_replayed(&self) {
        self.replayed.fetch_add(1, Ordering::Relaxed);
    }
    /// Record one `resumed` delivery.
    pub fn record_resumed(&self) {
        self.resumed.fetch_add(1, Ordering::Relaxed);
    }
    /// Record one `ignored_end` delivery.
    pub fn record_ignored_end(&self) {
        self.ignored_end.fetch_add(1, Ordering::Relaxed);
    }
    /// Record one `dropped_collision` delivery.
    pub fn record_dropped_collision(&self) {
        self.dropped_collision.fetch_add(1, Ordering::Relaxed);
    }
    /// Record one `failed` delivery.
    pub fn record_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }
    /// Stamp the moment an event reached durable storage.
    pub fn record_persisted(&self, unix_ms: u64) {
        self.last_persisted_ms.store(unix_ms, Ordering::Relaxed);
    }

    /// Read every counter for status reporting.
    #[must_use]
    pub fn snapshot(&self) -> IngestMetricsSnapshot {
        IngestMetricsSnapshot {
            accepted: self.accepted.load(Ordering::Relaxed),
            stored: self.stored.load(Ordering::Relaxed),
            replayed: self.replayed.load(Ordering::Relaxed),
            resumed: self.resumed.load(Ordering::Relaxed),
            ignored_end: self.ignored_end.load(Ordering::Relaxed),
            dropped_collision: self.dropped_collision.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),

            dropped_by_policy: self.dropped_by_policy.load(Ordering::Relaxed),
            dropped_unauthorized: self.dropped_unauthorized.load(Ordering::Relaxed),
            dropped_invalid: self.dropped_invalid.load(Ordering::Relaxed),
            shed_saturated: self.shed_saturated.load(Ordering::Relaxed),
            shed_rate_limited: self.shed_rate_limited.load(Ordering::Relaxed),
            last_persisted_ms: match self.last_persisted_ms.load(Ordering::Relaxed) {
                0 => None,
                ms => Some(ms),
            },
        }
    }
}

/// A read of [`IngestMetrics`], safe to serialise into status output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct IngestMetricsSnapshot {
    /// Events admitted for processing since this process started.
    pub accepted: u64,
    /// Events accepted but intentionally not stored.
    pub dropped_by_policy: u64,
    /// Captures dropped because their author may not write that repository.
    pub dropped_unauthorized: u64,
    /// Batch items dropped because they can never be stored (no session id,
    /// or a purged session).
    pub dropped_invalid: u64,
    /// Events shed because ingest capacity was exhausted.
    pub shed_saturated: u64,
    /// Events shed by the per-source rate limiter.
    pub shed_rate_limited: u64,
    /// Hook deliveries classified as `stored`.
    pub stored: u64,
    /// Hook deliveries classified as `replayed`.
    pub replayed: u64,
    /// Hook deliveries classified as `resumed`.
    pub resumed: u64,
    /// Hook deliveries classified as `ignored_end`.
    pub ignored_end: u64,
    /// Hook deliveries classified as `dropped_collision`.
    pub dropped_collision: u64,
    /// Hook deliveries classified as `failed`.
    pub failed: u64,
    /// Unix ms of the last newly persisted observation or effect, if any.
    pub last_persisted_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_start_at_zero_and_report_no_write() {
        let snap = IngestMetrics::default().snapshot();
        assert_eq!(snap.accepted, 0);
        assert_eq!(snap.dropped_by_policy, 0);
        assert_eq!(snap.dropped_unauthorized, 0);
        assert_eq!(snap.dropped_invalid, 0);
        assert_eq!(snap.shed_saturated, 0);
        assert_eq!(snap.shed_rate_limited, 0);
        assert_eq!(
            snap.last_persisted_ms, None,
            "a process that has written nothing must report None, not epoch 0"
        );
    }

    #[test]
    fn each_counter_moves_independently() {
        let m = IngestMetrics::default();
        m.record_accepted();
        m.record_accepted();
        m.record_dropped_by_policy();
        m.record_shed_saturated();
        m.record_shed_rate_limited();
        m.record_persisted(1_700_000_000_000);

        let s = m.snapshot();
        assert_eq!(s.accepted, 2);
        assert_eq!(s.dropped_by_policy, 1);
        assert_eq!(s.shed_saturated, 1);
        assert_eq!(s.shed_rate_limited, 1);
        assert_eq!(s.last_persisted_ms, Some(1_700_000_000_000));
    }

    #[test]
    fn additive_outcomes_do_not_change_admission_or_last_write() {
        let metrics = IngestMetrics::default();
        metrics.record_stored();
        metrics.record_replayed();
        metrics.record_resumed();
        metrics.record_ignored_end();
        metrics.record_dropped_collision();
        metrics.record_failed();
        let s = metrics.snapshot();
        assert_eq!(
            (s.stored, s.replayed, s.resumed, s.ignored_end),
            (1, 1, 1, 1)
        );
        assert_eq!((s.dropped_collision, s.failed), (1, 1));
        assert_eq!(s.accepted, 0);
        assert_eq!(s.dropped_by_policy, 0);
        assert_eq!(s.last_persisted_ms, None);
    }

    /// The shape is the privacy contract: counts and one timestamp, nothing
    /// that could carry a prompt, path, or tool payload. If a field carrying
    /// captured text is ever added, this fails loudly (#428).
    #[test]
    fn snapshot_is_content_free() {
        let m = IngestMetrics::default();
        m.record_persisted(1);
        let value = serde_json::to_value(m.snapshot()).expect("serialises");
        let mut keys: Vec<_> = value.as_object().expect("object").keys().cloned().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "accepted",
                "dropped_by_policy",
                "dropped_collision",
                "dropped_invalid",
                "dropped_unauthorized",
                "failed",
                "ignored_end",
                "last_persisted_ms",
                "replayed",
                "resumed",
                "shed_rate_limited",
                "shed_saturated",
                "stored"
            ]
        );
        for v in value.as_object().unwrap().values() {
            assert!(
                v.is_number() || v.is_null(),
                "every field must be a count or a timestamp, got {v}"
            );
        }
    }

    /// Counters are shared across tasks through an `Arc`; concurrent
    /// increments must not lose events.
    #[test]
    fn concurrent_increments_do_not_lose_events() {
        use std::sync::Arc;
        let m = Arc::new(IngestMetrics::default());
        let mut handles = Vec::new();
        for _ in 0..8 {
            let m = Arc::clone(&m);
            handles.push(std::thread::spawn(move || {
                for _ in 0..1000 {
                    m.record_accepted();
                }
            }));
        }
        for h in handles {
            h.join().expect("thread");
        }
        assert_eq!(m.snapshot().accepted, 8000);
    }
}
