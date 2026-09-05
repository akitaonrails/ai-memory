//! This crate's integration tests. Every file here is a module of the lib's
//! test harness (see the `integration` module in `src/lib.rs`), so they cost
//! no extra binary; a new file must be declared below.

mod access_breadth_sweep;
mod embed_backfill;
mod embeddings;
mod lifecycle;
mod local_embeddings;
mod multi_machine;
mod observation_retention;
mod recall_eval;
mod search_quality;
mod typed_edges;
