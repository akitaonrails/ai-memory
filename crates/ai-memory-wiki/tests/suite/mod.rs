//! This crate's integration tests. Every file here is a module of the lib's
//! test harness (see the `integration` module in `src/lib.rs`), so they cost
//! no extra binary; a new file must be declared below.

mod admission;
mod admission_bot_wake;
mod auto_improve_admission;
mod ledger_files;
mod wiki_git_ledgers;
