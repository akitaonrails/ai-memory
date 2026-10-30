//! Conservative per-stage budgets of the chunked path.
//!
//! The new path never reuses the legacy consolidation scaffolding (the 19k
//! character batch prompt plus the full page schema): map, reduce and final
//! calls each carry their own compact system prompt and schema, and every
//! character of them is charged to `chunking.max_input_tokens` before any
//! evidence is admitted. [`StageBudgets::derive`] runs at enable time over the
//! complete path, so a budget that cannot hold one minimal evidence slice, two
//! digests, or the final envelope fails there, not mid-job.

use ai_memory_core::ProcessingJobKind;
use sha2::{Digest, Sha256};

use super::chunk_digest::{ChunkDigest, MAP_SYSTEM_PROMPT, REDUCE_SYSTEM_PROMPT, schema_cost};
use super::chunk_pipeline::GROUP_HEADER_COST;
use super::chunk_plan::{MIN_CHUNK_EVIDENCE_COST, hex, text_cost};
use crate::auto_improve::AutoImproveLlmResponse;
use crate::types::{ConsolidatedBatch, ConsolidatedPage};

pub(super) const FINAL_PAGE_SYSTEM_PROMPT: &str =
    include_str!("../../prompts/chunk_final_page_system.md");
pub(super) const FINAL_BATCH_SYSTEM_PROMPT: &str =
    include_str!("../../prompts/chunk_final_batch_system.md");
pub(super) const FINAL_AUTO_IMPROVE_SYSTEM_PROMPT: &str =
    include_str!("../../prompts/chunk_final_auto_improve_system.md");

/// Bumped whenever prompts, schemas or planning change meaning.
pub(super) const PIPELINE_VERSION: &str = "chunk-v1";
/// Chat-envelope allowance (roles, separators, provider framing).
pub(super) const ENVELOPE_COST: usize = 1_024;
/// Allowance for the user-message wrapper text of map and reduce calls.
pub(super) const MAP_WRAPPER_COST: usize = 700;
/// Allowance for the fixed user-message text of a final call.
pub(super) const FINAL_WRAPPER_COST: usize = 1_100;
/// Output ceiling of a final call, however large `[consolidation]` allows.
pub(super) const FINAL_OUTPUT_TOKEN_CAP: u32 = 8_192;
/// Smallest useful per-node digest.
const MIN_DIGEST_COST: usize = 1_200;
const MIN_OUTPUT_TOKENS: u32 = 1_000;
const CHARS_PER_TOKEN: f64 = 3.0;
const MAX_FINAL_CONTEXT_COST: usize = 6_000;
/// A final call must still fit one full digest plus this much slack.
const FINAL_DIGEST_SLACK: usize = 200;

/// Characters a call may cost for `tokens` of input at `margin`.
#[must_use]
pub(super) fn call_cost(tokens: usize, margin: f64) -> usize {
    (tokens as f64 * CHARS_PER_TOKEN * margin.clamp(0.05, 1.0)) as usize
}

fn tokens_for(cost: usize, margin: f64) -> usize {
    (cost as f64 / (CHARS_PER_TOKEN * margin.clamp(0.05, 1.0))).ceil() as usize
}

/// SHA-256 over length-prefixed parts, lower-case hex.
#[must_use]
pub(super) fn fingerprint<S: AsRef<str>>(parts: &[S]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        let part = part.as_ref();
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hex(&hasher.finalize())
}

/// Fingerprint of the JSON schema of `T`.
pub(super) fn schema_fingerprint<T: schemars::JsonSchema>() -> String {
    fingerprint(&[&serde_json::to_string(&schemars::schema_for!(T)).unwrap_or_default()])
}

/// Prompt and schema of the final call of `kind`, with its schema cost.
pub(super) fn final_prompt(kind: ProcessingJobKind) -> (&'static str, usize) {
    match kind {
        ProcessingJobKind::Consolidate => {
            (FINAL_PAGE_SYSTEM_PROMPT, schema_cost::<ConsolidatedPage>())
        }
        ProcessingJobKind::ConsolidateMulti => (
            FINAL_BATCH_SYSTEM_PROMPT,
            schema_cost::<ConsolidatedBatch>(),
        ),
        ProcessingJobKind::AutoImprove => (
            FINAL_AUTO_IMPROVE_SYSTEM_PROMPT,
            schema_cost::<AutoImproveLlmResponse>(),
        ),
    }
}

/// Every budget one job derives from the three operator knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StageBudgets {
    /// Total conservative input cost of one call.
    pub call_cost: usize,
    /// Evidence characters of one map chunk.
    pub map_evidence_cap: usize,
    /// Digest characters of one reduce group.
    pub reduce_cap: usize,
    /// Largest digest one node may persist.
    pub digest_cap: usize,
    /// Output ceiling of map calls.
    pub map_out_tokens: u32,
    /// Output ceiling of reduce calls.
    pub reduce_out_tokens: u32,
    /// Output ceiling of the final call.
    pub final_out_tokens: u32,
    /// Digest characters the final call may carry.
    pub final_digest_cap: usize,
    /// Optional-context characters (pages, titles, slots) of the final call.
    pub final_context_cap: usize,
}

impl StageBudgets {
    /// Derive and validate the whole path.
    ///
    /// `call_tokens` is `min(chunking.max_input_tokens, the operation's own
    /// input limit)`; `op_max_output` is the operation's output limit.
    ///
    /// # Errors
    /// A message naming the knob that must grow.
    pub(super) fn derive(
        kind: ProcessingJobKind,
        call_tokens: usize,
        margin: f64,
        op_max_output: u32,
    ) -> Result<Self, String> {
        let call = call_cost(call_tokens, margin);
        let schema = schema_cost::<ChunkDigest>();

        let map_fixed = text_cost(MAP_SYSTEM_PROMPT) + schema + MAP_WRAPPER_COST + ENVELOPE_COST;
        let map_floor = map_fixed + 2 * MIN_CHUNK_EVIDENCE_COST;
        let map_evidence_cap = call
            .checked_sub(map_fixed)
            .filter(|room| *room >= 2 * MIN_CHUNK_EVIDENCE_COST)
            .ok_or_else(|| {
                format!(
                    "chunking.max_input_tokens too small: the map call needs at least {} tokens \
                     for its prompt, schema and one evidence slice",
                    tokens_for(map_floor, margin)
                )
            })?;

        let reduce_fixed =
            text_cost(REDUCE_SYSTEM_PROMPT) + schema + MAP_WRAPPER_COST + ENVELOPE_COST;
        let reduce_floor = reduce_fixed + 3 * MIN_DIGEST_COST;
        let reduce_cap = call
            .checked_sub(reduce_fixed)
            .filter(|room| *room >= 3 * MIN_DIGEST_COST)
            .ok_or_else(|| {
                format!(
                    "chunking.max_input_tokens too small: the reduce call needs at least {} \
                     tokens to merge three digests",
                    tokens_for(reduce_floor, margin)
                )
            })?;
        let digest_cap = reduce_cap / 3;

        let digest_out = u32::try_from(digest_cap / 2 + 256).unwrap_or(u32::MAX);
        if op_max_output < digest_out.max(MIN_OUTPUT_TOKENS) {
            return Err(format!(
                "output limit {op_max_output} tokens is too small for chunk digests \
                 (needs {})",
                digest_out.max(MIN_OUTPUT_TOKENS)
            ));
        }

        let (system, final_schema) = final_prompt(kind);
        let final_fixed = text_cost(system) + final_schema + FINAL_WRAPPER_COST + ENVELOPE_COST;
        let final_room = call.checked_sub(final_fixed).ok_or_else(|| {
            format!(
                "chunking.max_input_tokens too small: the final {} call needs at least {} \
                 tokens for its prompt and schema",
                kind.as_str(),
                tokens_for(
                    final_fixed + digest_cap + GROUP_HEADER_COST + FINAL_DIGEST_SLACK,
                    margin
                )
            )
        })?;
        let final_context_cap = (final_room * 2 / 5).min(MAX_FINAL_CONTEXT_COST);
        let final_digest_cap = final_room - final_context_cap;
        if final_digest_cap < digest_cap + GROUP_HEADER_COST + FINAL_DIGEST_SLACK {
            return Err(format!(
                "chunking.max_input_tokens too small: the final {} call cannot carry one \
                 full digest (needs at least {} tokens)",
                kind.as_str(),
                tokens_for(
                    final_fixed
                        + digest_cap
                        + GROUP_HEADER_COST
                        + FINAL_DIGEST_SLACK
                        + final_context_cap,
                    margin
                )
            ));
        }
        let final_out_tokens = op_max_output.min(FINAL_OUTPUT_TOKEN_CAP);
        if final_out_tokens < MIN_OUTPUT_TOKENS {
            return Err("output limit is too small for a final page".into());
        }
        Ok(Self {
            call_cost: call,
            map_evidence_cap,
            reduce_cap,
            digest_cap,
            map_out_tokens: digest_out.min(op_max_output),
            reduce_out_tokens: digest_out.min(op_max_output),
            final_out_tokens,
            final_digest_cap,
            final_context_cap,
        })
    }

    /// Final digest room after `shrinks` recoveries from a truncated or
    /// rejected final call (each halves it, never below one digest).
    #[must_use]
    pub(super) fn final_cap_after(&self, shrinks: u32) -> usize {
        (self.final_digest_cap >> shrinks.min(8)).max(self.digest_cap + GROUP_HEADER_COST)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_budget_holds_every_stage_of_every_kind() {
        for kind in [
            ProcessingJobKind::Consolidate,
            ProcessingJobKind::ConsolidateMulti,
            ProcessingJobKind::AutoImprove,
        ] {
            let b = StageBudgets::derive(kind, 8_192, 0.8, 32_000)
                .unwrap_or_else(|e| panic!("{kind:?}: {e}"));
            assert!(b.map_evidence_cap >= 2 * MIN_CHUNK_EVIDENCE_COST);
            assert!(b.digest_cap * 3 <= b.reduce_cap);
            assert!(b.final_digest_cap >= b.digest_cap + GROUP_HEADER_COST);
            assert!(b.final_out_tokens <= FINAL_OUTPUT_TOKEN_CAP);
            assert!(b.map_out_tokens <= 32_000);
            // the legacy 32k output allowance is never inherited
            assert!(b.map_out_tokens < 8_192);
        }
    }

    #[test]
    fn a_budget_that_cannot_hold_the_path_names_the_knob() {
        let err = StageBudgets::derive(ProcessingJobKind::ConsolidateMulti, 2_048, 0.8, 32_000)
            .unwrap_err();
        assert!(err.contains("chunking.max_input_tokens"), "{err}");
        let err =
            StageBudgets::derive(ProcessingJobKind::Consolidate, 8_192, 0.8, 1_000).unwrap_err();
        assert!(err.contains("output limit"), "{err}");
    }

    #[test]
    fn shrinking_the_final_room_never_goes_below_one_digest() {
        let b = StageBudgets::derive(ProcessingJobKind::Consolidate, 8_192, 0.8, 32_000).unwrap();
        assert!(b.final_cap_after(1) <= b.final_cap_after(0));
        assert_eq!(b.final_cap_after(20), b.digest_cap + GROUP_HEADER_COST);
    }

    #[test]
    fn fingerprints_are_length_prefixed() {
        assert_ne!(fingerprint(&["ab", "c"]), fingerprint(&["a", "bc"]));
    }
}
