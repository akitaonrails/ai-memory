//! `ai-memory` binary entry point.
//!
//! Deliberately thin: all logic lives in the `ai_memory_cli` lib target so it
//! is unit-testable and linkable. See that crate's docs for the dispatch flow.

#![doc(html_no_source)]

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    ai_memory_cli::run().await
}
