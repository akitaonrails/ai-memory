//! This crate's integration tests. Every file here is a module of the lib's
//! test harness (see the `integration` module in `src/lib.rs`), so they cost
//! no extra binary; a new file must be declared below.

mod escaped_json_secret;
mod idle_end;
mod powershell_home;
mod powershell_marker_aliases;
mod powershell_server_routed;
mod powershell_spool;
mod powershell_utf8;
