//! This crate's integration tests. Every file here is a module of the lib's
//! test harness (see the `integration` module in `src/lib.rs`), so they cost
//! no extra binary; a new file must be declared below.

mod abstract_backfill;
mod access_breadth_sweep;
mod aging_lifecycle;
mod cold_cluster_sweep;
mod compaction_sweep;
mod contradiction_lint;
mod dream_pass;
mod embed_backfill;
mod embeddings;
mod entropy_experience;
mod lifecycle;
mod local_embeddings;
mod multi_machine;
mod observation_retention;
mod recall_eval;
mod search_quality;
mod typed_edges;
