//! `ai-memory` binary entry point.
//!
//! Deliberately thin: all logic lives in the `ai_memory_cli` lib target so it
//! is unit-testable and linkable. See that crate's docs for the dispatch flow.

#![doc(html_no_source)]

use std::time::Duration;

use anyhow::Result;

fn main() -> Result<()> {
    // The runtime is built by hand rather than with `#[tokio::main]` for the
    // sake of the `shutdown_timeout` below; the builder settings are the ones
    // that attribute would have used.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(ai_memory_cli::run());
    // `run` has returned, so every guard it owns — the log-flush guard, the
    // store, the wiki watcher, the serve lock — has already been dropped in
    // order. What can still be parked is the MCP stdio transport's read of
    // stdin: tokio serves it from a blocking thread, a blocking read cannot be
    // cancelled, and dropping a runtime waits for in-flight blocking work
    // forever. That await is what kept `serve --transport stdio` alive after a
    // handled Ctrl-C (#699). Nothing is waiting on that read's result any more,
    // so stop waiting for it and let the process exit.
    runtime.shutdown_timeout(Duration::ZERO);
    result
}
