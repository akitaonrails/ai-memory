//! `ai-memory` binary entry point.
//!
//! Deliberately thin: all logic lives in the `ai_memory_cli` lib target so it
//! is unit-testable and linkable. See that crate's docs for the dispatch flow.

#![doc(html_no_source)]

use std::{process::ExitCode, time::Duration};

use anyhow::Result;

fn main() -> ExitCode {
    match start() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{}", ai_memory_cli::failure_report(&error));
            ExitCode::FAILURE
        }
    }
}

fn start() -> Result<()> {
    #[cfg(windows)]
    {
        // Windows executables start with a much smaller main-thread stack than
        // Unix processes. The CLI dispatch future contains every subcommand
        // branch and can outgrow that default as the command tree expands.
        // Keep the platform default for worker threads, but give the one
        // thread that owns the top-level future enough room to dispatch it.
        std::thread::Builder::new()
            .name("ai-memory-main".to_owned())
            .stack_size(8 * 1024 * 1024)
            .spawn(run)?
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }

    #[cfg(not(windows))]
    run()
}

fn run() -> Result<()> {
    // The runtime is built by hand rather than with `#[tokio::main]` for the
    // sake of the `shutdown_timeout` below; the builder settings are the ones
    // that attribute would have used, except for the flavor: a hook call is one
    // short request, and the multi-thread builder would spawn a worker per CPU
    // to run it (#1669).
    let argv: Vec<String> = std::env::args().collect();
    let mut builder = match ai_memory_cli::runtime_flavor::RuntimeFlavor::for_argv(&argv) {
        ai_memory_cli::runtime_flavor::RuntimeFlavor::CurrentThread => {
            tokio::runtime::Builder::new_current_thread()
        }
        ai_memory_cli::runtime_flavor::RuntimeFlavor::MultiThread => {
            tokio::runtime::Builder::new_multi_thread()
        }
    };
    let runtime = builder.enable_all().build()?;
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
