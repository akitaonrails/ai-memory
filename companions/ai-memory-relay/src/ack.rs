//! Acknowledgement parsing for `POST /hook/batch`.
//!
//! Validate the entire response before releasing any queued event. An
//! inconsistent acknowledgement leaves the whole batch pending for retry.

use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;

/// Bounded receipt and report vocabulary. Future server strings become unknown.
pub const OUTCOMES: [&str; 9] = [
    "stored",
    "replayed",
    "resumed",
    "ignored_end",
    "dropped_policy",
    "dropped_subagent",
    "dropped_unauthorized",
    "dropped_collision",
    "unknown",
];

/// Normalize an outcome without retaining arbitrary server text.
pub fn outcome(value: &str) -> &'static str {
    OUTCOMES
        .iter()
        .copied()
        .find(|known| *known == value)
        .unwrap_or("unknown")
}

/// Fixed keys, including zero counts.
pub fn outcome_counts() -> BTreeMap<String, i64> {
    OUTCOMES
        .into_iter()
        .map(|key| (key.to_owned(), 0))
        .collect()
}

/// One acknowledged event's disposition.
#[derive(Debug, Clone, Deserialize)]
pub struct EventResult {
    pub index: usize,
    pub outcome: String,
}

fn present_results<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<EventResult>>, D::Error> {
    Vec::<EventResult>::deserialize(d).map(Some)
}

/// The server's ack. Unknown fields are accepted (the server may add some);
/// a missing `accepted` is a malformed ack and preserves the batch.
#[derive(Debug, Clone, Deserialize)]
pub struct BatchAck {
    /// Contiguous leading prefix committed, oldest-first.
    pub accepted: usize,
    /// Non-contiguous committed indexes, when per-source rate limiting skipped items.
    #[serde(default)]
    pub accepted_indices: Option<Vec<usize>>,
    /// Item that failed processing after earlier skips.
    #[serde(default)]
    pub failed_index: Option<usize>,
    /// Missing on old servers; present arrays must describe every acknowledged index.
    #[serde(default, deserialize_with = "present_results")]
    pub results: Option<Vec<EventResult>>,
}

/// Why an ack was refused. The batch stays pending in every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckRejected(pub String);

impl std::fmt::Display for AckRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Validate an ack against the batch it answers, returning the indexes that may
/// be released.
///
/// `accepted` is the contiguous leading prefix *even when* `accepted_indices` is
/// present, so the two must agree. Duplicated or out-of-range indexes, a
/// disagreeing `accepted`, a `failed_index` outside the batch, and a
/// `failed_index` that also claims to be accepted are all refusals.
pub fn validate(batch_len: usize, ack: &BatchAck) -> Result<Vec<usize>, AckRejected> {
    let reject = |detail: String| Err(AckRejected(detail));
    let accepted: Vec<usize> = match &ack.accepted_indices {
        Some(indices) => {
            if let Some(bad) = indices.iter().find(|idx| **idx >= batch_len) {
                return reject(format!(
                    "accepted_indices contains {bad}, outside a {batch_len}-item batch"
                ));
            }
            if indices.windows(2).any(|pair| pair[0] >= pair[1]) {
                return reject(
                    "accepted_indices must be strictly ascending and duplicate-free".into(),
                );
            }
            let prefix = indices
                .iter()
                .enumerate()
                .take_while(|(pos, idx)| pos == *idx)
                .count();
            if prefix != ack.accepted {
                return reject(format!(
                    "accepted={} disagrees with the {prefix}-item contiguous prefix of accepted_indices",
                    ack.accepted
                ));
            }
            indices.clone()
        }
        None => {
            if ack.accepted > batch_len {
                return reject(format!(
                    "accepted={} exceeds the {batch_len} items sent",
                    ack.accepted
                ));
            }
            (0..ack.accepted).collect()
        }
    };
    if let Some(failed) = ack.failed_index {
        if failed >= batch_len {
            return reject(format!(
                "failed_index={failed} is outside a {batch_len}-item batch"
            ));
        }
        if accepted.contains(&failed) {
            return reject(format!(
                "failed_index={failed} is also reported as accepted"
            ));
        }
        // The server fails fast: it stops at `failed_index` and processes
        // nothing after it. An ack claiming a later item committed describes a
        // run that cannot have happened, so the batch is preserved whole.
        if let Some(after) = accepted.iter().find(|idx| **idx > failed) {
            return reject(format!(
                "accepted index {after} comes after failed_index={failed}, which the server \
                 never processes past"
            ));
        }
    }
    if let Some(results) = &ack.results
        && (results.len() != accepted.len()
            || results
                .iter()
                .zip(&accepted)
                .any(|(result, index)| result.index != *index))
    {
        return reject(
            "results must match acknowledged indices exactly, in ascending order".into(),
        );
    }
    Ok(accepted)
}

/// Outcome for an index after successful whole-response validation.
pub fn acknowledged_outcome(ack: &BatchAck, index: usize) -> &'static str {
    ack.results
        .as_ref()
        .and_then(|results| results.iter().find(|r| r.index == index))
        .map(|result| outcome(&result.outcome))
        .unwrap_or("unknown")
}
