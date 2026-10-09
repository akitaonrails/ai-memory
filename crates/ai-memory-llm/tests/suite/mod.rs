//! This crate's integration tests. Every file here is a module of the lib's
//! test harness (see the `integration` module in `src/lib.rs`), so they cost
//! no extra binary; a new file must be declared below.

mod copilot_embedder;
mod copilot_provider;
mod cursor_provider;
mod extra_headers_on_the_wire;
mod fallback_provider;
mod openai_compat_embedder;
mod openai_compat_guardrails;
mod openai_compat_strict;
