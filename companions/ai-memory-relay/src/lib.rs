//! Persistent queue for external lifecycle events sent through `POST /hook/batch`.
//!
//! The queue is separate from ai-memory's database and wiki. Events leave it
//! after a validated acknowledgement, which may include a server policy drop.
//! Pending events remain on disk when delivery fails or their retry window
//! expires; reaching a capacity limit rejects new input.
//!
//! Bodies retain their input values before server sanitization. Producers must
//! apply capture exclusions before enqueueing and protect the queue directory.

pub mod ack;
pub mod fsguard;
pub mod http;
pub mod identity;
pub mod queue;
pub mod relay;

/// Wall-clock milliseconds since the Unix epoch.
///
/// Used for the retry window, which is why it is clamped at 0 rather than
/// panicking on a pre-epoch clock: a nonsensical clock must not take the process
/// down mid-flush.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
