//! This crate's integration tests. Every file here is a module of the lib's
//! test harness (see the `integration` module in `src/lib.rs`), so they cost
//! no extra binary; a new file must be declared below.

mod common;

mod admin_audit_log;
mod admin_backup;
mod admin_bootstrap;
mod admin_move;
mod admin_move_session;
mod admin_phase3;
mod admin_purge;
mod admin_read_page;
mod admin_rename;
mod admin_status_search;
mod admin_write_page;
mod autoscope_multiuser;
mod handoff_admission;
mod handoff_identity;
mod mcp_stateless_http;
mod slot_identity;
mod stress_autoscope;
