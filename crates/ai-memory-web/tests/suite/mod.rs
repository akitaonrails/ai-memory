//! This crate's integration tests. Every file here is a module of the lib's
//! test harness (see the `integration` module in `src/lib.rs`), so they cost
//! no extra binary; a new file must be declared below.

mod documentation;
mod graph;
mod health;
mod history_replay;
mod operations;
mod pending;
mod recall_trace;
mod routes;
mod workstreams;

#[cfg(feature = "management-ui")]
mod management;

mod contextual;

mod localization;
