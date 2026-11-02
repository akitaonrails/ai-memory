//! This crate's integration tests. Every file here is a module of the lib's
//! test harness (see the `integration` module in `src/lib.rs`), so they cost
//! no extra binary; a new file must be declared below.

mod abstract_backfill;
mod access_breadth_sweep;
mod aging_lifecycle;
mod bounded_session_load;
mod cold_cluster_sweep;
mod compaction_sweep;
mod contradiction_lint;
mod dream_pass;
mod embed_backfill;
mod embeddings;
mod entropy_experience;
#[cfg(unix)]
mod eval_results;
mod ledger_sweep;
mod lifecycle;
mod local_embeddings;
mod multi_machine;
mod observation_retention;
mod profile_harvest;
mod recall_eval;
mod rule_slug_collisions;
mod search_quality;
mod typed_edges;
mod verbatim_residue_consolidation;
