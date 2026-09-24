//! Shared "is another ai-memory process alive?" check.
//!
//! Used by direct-disk lifecycle operations that must not race a live writer:
//! `reset`, `restore`, `reindex`, and `uninstall` when `--purge-data` is set
//! (lesson from basic-memory #765). `backup` is intentionally excluded: it is a
//! thin HTTP client, and the server snapshots SQLite through its online backup
//! API while the writer stays live.

use std::ffi::OsStr;

use sysinfo::System;

/// Binary name to match against `/proc/*/comm` (or platform equivalent).
pub const BIN_NAME: &str = crate::install_layout::BINARY_NAME;

/// Return PIDs of *other* `ai-memory` processes (excluding the current
/// process and any threads of it).
#[must_use]
pub fn sibling_processes() -> Vec<sysinfo::Pid> {
    // Test injection: a comma-separated list of fake PIDs to report as alive
    // siblings, bypassing both the real scan AND the `cfg!(test)` opt-out
    // below. This is what lets the guard's REFUSAL path be exercised by an
    // in-process test (reset / reindex / restore / uninstall --purge-data all
    // call `sibling_processes()` directly, and `cfg!(test)` alone would
    // otherwise force every in-process test onto the "no siblings" branch).
    // Checked first, and not itself gated by `cfg!(test)`, matching the
    // existing `AI_MEMORY_TEST_NO_PROCESS_GUARD` opt-out below: neither is
    // reachable in a normal shipped run because neither is ever set outside
    // a test harness's own env.
    if let Ok(raw) = std::env::var("AI_MEMORY_TEST_FORCE_SIBLING_PIDS") {
        return raw
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .filter_map(|s| s.trim().parse::<u32>().ok())
            .map(sysinfo::Pid::from_u32)
            .collect();
    }
    // Test opt-out. The destructive-command tests would otherwise flake
    // non-deterministically: a dev box (and a parallel test run) almost always
    // has some *other* `ai-memory` process alive, which the real scan rightly
    // refuses against. Two seams, neither reachable in a normal shipped run:
    //   - `cfg!(test)` — the crate's own in-process unit tests (reset / reindex
    //     / restore) skip the scan.
    //   - `AI_MEMORY_TEST_NO_PROCESS_GUARD` — a spawned `ai-memory` a test
    //     launches reads it from its env and skips. The dedicated, `#[ignore]`d
    //     guard test launches WITHOUT it, so the guard itself stays covered.
    if cfg!(test) || std::env::var_os("AI_MEMORY_TEST_NO_PROCESS_GUARD").is_some() {
        return Vec::new();
    }
    let mut sys = System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let me = sysinfo::Pid::from_u32(std::process::id());
    let bin_os: &OsStr = OsStr::new(BIN_NAME);
    sys.processes_by_exact_name(bin_os)
        // On Linux, sysinfo lists tokio worker threads alongside the main
        // process under the same comm name. thread_kind() == None means
        // we're looking at the process leader, not one of its threads.
        .filter(|p| p.thread_kind().is_none())
        .map(sysinfo::Process::pid)
        .filter(|pid| *pid != me)
        .collect()
}

/// Format a "refusing to ..." error message for the given operation,
/// quoting sibling PIDs.
#[must_use]
pub fn busy_message(verb: &str, siblings: &[sysinfo::Pid]) -> String {
    let pids: Vec<u32> = siblings.iter().copied().map(sysinfo::Pid::as_u32).collect();
    format!(
        "refusing to {}: {} other ai-memory process(es) running (pids: {:?}). \
         Stop them first, then re-run.",
        verb,
        pids.len(),
        pids,
    )
}
