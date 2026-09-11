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
    // `run` has returned and dropped the handles it owned, but the store
    // writer's `Shutdown`-and-join does NOT run here: the maintenance
    // scheduler's detached tasks (and, once a client has connected, the
    // client-activity flush loop) hold surviving `WriterHandle` clones, so
    // `WriterInner::drop` never fires and `shutdown_timeout(ZERO)` abandons
    // those tasks rather than letting them finish. Writes still queued on the
    // writer at signal time are dropped — the same abruptness an un-handled
    // interrupt already imposes on a server that is not PID 1 (#703, #710).
    // What this DOES cure is the stdio hang: the MCP stdio transport reads
    // stdin on a blocking thread that cannot be cancelled, and dropping the
    // runtime would otherwise wait for that read forever, which is what kept
    // `serve --transport stdio` alive after a handled Ctrl-C (#699). Nothing
    // awaits that read's result any more, so stop waiting and let the process
    // exit.
    runtime.shutdown_timeout(Duration::ZERO);
    result
}
