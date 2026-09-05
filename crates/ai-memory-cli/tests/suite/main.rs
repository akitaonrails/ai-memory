//! Single binary for this crate's integration tests.
//!
//! Every file in this directory is a module of this one test binary: one
//! link per rebuild instead of one per file. Cargo treats `tests/suite/main.rs`
//! as the single `suite` target and never builds the sibling files on their own,
//! so a new file must be declared here (`scripts/check-test-suites.*` enforces it).

mod autoscope_env;
mod completions;
mod hook_drain;
mod hook_payload;
mod marker_scope;
mod packaging;
mod removal;
mod repo_layout;
mod routing_instructions;
mod routing_skills;
