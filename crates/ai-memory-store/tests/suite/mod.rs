//! This crate's integration tests. Every file here is a module of the lib's
//! test harness (see the `integration` module in `src/lib.rs`), so they cost
//! no extra binary; a new file must be declared below.

mod access_breadth;
mod audit_contamination;
mod audit_log;
mod auto_improve_staging;
mod client_activity;
mod fts_drift_status;
mod handoff_ownership;
mod multi_session;
mod session_ids_touching_scope;
mod session_observations;
mod session_scope_from_observations;
mod sessions_by_agent;
mod slot_visibility;
mod stress_writer_throughput;
