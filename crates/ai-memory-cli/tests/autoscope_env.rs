//! Subprocess smoke tests for `[auto_scope]` env-var configuration.
//!
//! These tests assert the wiring between figment's `AI_MEMORY_*` env-var
//! parser and the `[auto_scope]` config block. The chart values.yaml will
//! ship the same env vars (`AI_MEMORY_AUTO_SCOPE__MODE`, …), so a regression
//! here would silently downgrade prod from `per_session`/`per_actor` to
//! the legacy `single` slot — and we'd only catch it by smoke-running
//! against a multi-actor scenario in prod.
//!
//! Each test spawns the `ai-memory` binary with one env-var combination,
//! waits for the startup log line that records the resolved mode, and
//! kills the child. Invalid values trip a clear-error exit, which is
//! also asserted so figment's parse-failure path stays loud.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

/// Spawn the binary with the given env vars and stream stderr until either
/// `needle` appears in a line or `timeout` elapses. Always kills the child
/// before returning. Returns `(matched_line_or_none, all_stderr_captured)`.
fn spawn_and_wait_for_log(
    envs: &[(&str, &str)],
    needle: &str,
    timeout: Duration,
) -> (Option<String>, String) {
    let tmp = TempDir::new().expect("tempdir for serve");
    // `--bind 127.0.0.1:0` asks the OS for any free port — we never
    // actually want to talk to the listener, only to confirm the engine
    // starts up far enough to log the `[auto_scope]` mode.
    let mut cmd = Command::new(bin());
    cmd.args([
        "serve",
        "--transport",
        "http",
        "--bind",
        "127.0.0.1:0",
        "--data-dir",
    ])
    .arg(tmp.path())
    .env("AI_MEMORY_DATA_DIR", tmp.path())
    // Force tracing to spit at least info-level so the
    // "active-project isolation mode" line is rendered to stderr.
    .env("RUST_LOG", "info")
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn ai-memory serve");

    // Pump stderr in a background thread so we can apply a wall-clock
    // timeout on the *match*, not on the underlying read syscalls.
    let stderr = child.stderr.take().expect("stderr piped");
    let (tx, rx) = mpsc::channel::<String>();
    let pump = std::thread::spawn(move || {
        let mut all = String::new();
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            all.push_str(&line);
            all.push('\n');
            if tx.send(line).is_err() {
                break;
            }
        }
        all
    });

    let deadline = Instant::now() + timeout;
    let mut matched: Option<String> = None;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if line.contains(needle) {
                    matched = Some(line);
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    // Pump thread terminates once stderr closes (post-kill); collect what
    // it captured.
    let all_stderr = pump.join().unwrap_or_default();
    (matched, all_stderr)
}

/// Spawn the binary and let it run to completion, capturing both exit status
/// and stderr. Used for the invalid-mode test where startup fails before
/// reaching the log line we'd otherwise wait for.
fn spawn_and_wait_for_exit(envs: &[(&str, &str)], timeout: Duration) -> (bool, String) {
    let tmp = TempDir::new().expect("tempdir for serve");
    let mut cmd = Command::new(bin());
    cmd.args([
        "serve",
        "--transport",
        "http",
        "--bind",
        "127.0.0.1:0",
        "--data-dir",
    ])
    .arg(tmp.path())
    .env("AI_MEMORY_DATA_DIR", tmp.path())
    .env("RUST_LOG", "info")
    .stdout(Stdio::null())
    .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn ai-memory serve");

    // Poll for exit with a wall-clock cap. If the binary stays alive
    // past the timeout, that's *also* a failure for "invalid value
    // must fail fast" — kill it so the test doesn't hang and report
    // false success.
    let deadline = Instant::now() + timeout;
    let exited_cleanly = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break true; // still running == didn't fail fast
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break true,
        }
    };

    let mut stderr_buf = String::new();
    if let Some(mut s) = child.stderr.take() {
        use std::io::Read;
        let _ = s.read_to_string(&mut stderr_buf);
    }
    (exited_cleanly, stderr_buf)
}

const STARTUP_NEEDLE: &str = "active-project isolation mode";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(8);

/// The shipped default keys the active-project pointer by whatever coordinate
/// the caller has, so two harnesses in one project — or two operators on one
/// server — do not overwrite each other's pointer.
///
/// `Single` is still selectable, but it is no longer what an install gets for
/// free: a process-wide last-write-wins slot is only correct for exactly one
/// harness at a time, and nothing about a fresh install says that is the case.
#[test]
fn default_mode_is_per_actor() {
    let (line, all) = spawn_and_wait_for_log(&[], STARTUP_NEEDLE, STARTUP_TIMEOUT);
    let line = line.unwrap_or_else(|| panic!("startup log line not found.\nstderr:\n{all}"));
    assert!(
        line.contains("PerActor"),
        "default mode must log `PerActor`. Got: {line}\nfull stderr:\n{all}"
    );
}

/// …and `Single` must remain reachable for anyone who wants the old slot.
#[test]
fn single_mode_is_still_selectable_via_env() {
    let (line, all) = spawn_and_wait_for_log(
        &[("AI_MEMORY_AUTO_SCOPE__MODE", "single")],
        STARTUP_NEEDLE,
        STARTUP_TIMEOUT,
    );
    let line = line.unwrap_or_else(|| panic!("startup log line not found.\nstderr:\n{all}"));
    assert!(
        line.contains("Single"),
        "explicit single must log `Single`. Got: {line}\nfull stderr:\n{all}"
    );
}

#[test]
fn per_session_mode_via_env() {
    let (line, all) = spawn_and_wait_for_log(
        &[("AI_MEMORY_AUTO_SCOPE__MODE", "per_session")],
        STARTUP_NEEDLE,
        STARTUP_TIMEOUT,
    );
    let line = line
        .unwrap_or_else(|| panic!("startup log line not found for per_session.\nstderr:\n{all}"));
    assert!(
        line.contains("PerSession"),
        "expected `PerSession` in startup line, got: {line}\nfull stderr:\n{all}"
    );
}

#[test]
fn per_actor_mode_via_env() {
    let (line, all) = spawn_and_wait_for_log(
        &[("AI_MEMORY_AUTO_SCOPE__MODE", "per_actor")],
        STARTUP_NEEDLE,
        STARTUP_TIMEOUT,
    );
    let line =
        line.unwrap_or_else(|| panic!("startup log line not found for per_actor.\nstderr:\n{all}"));
    assert!(
        line.contains("PerActor"),
        "expected `PerActor` in startup line, got: {line}\nfull stderr:\n{all}"
    );
}

#[test]
fn ttl_and_max_entries_via_env() {
    // Confirms the full struct round-trips through figment's `__`
    // separator convention — not just `mode`. A typo in any of the
    // three field-names downstream would surface here.
    let (line, all) = spawn_and_wait_for_log(
        &[
            ("AI_MEMORY_AUTO_SCOPE__MODE", "per_session"),
            ("AI_MEMORY_AUTO_SCOPE__SESSION_TTL_SECS", "120"),
            ("AI_MEMORY_AUTO_SCOPE__MAX_ENTRIES", "64"),
        ],
        STARTUP_NEEDLE,
        STARTUP_TIMEOUT,
    );
    let line = line.unwrap_or_else(|| panic!("startup log not found.\nstderr:\n{all}"));
    assert!(
        line.contains("PerSession") && line.contains("120") && line.contains("64"),
        "all three fields must be reflected in the startup log.\nGot: {line}\nfull stderr:\n{all}"
    );
}

#[test]
fn invalid_mode_value_fails_fast() {
    // figment + serde's `rename_all = "snake_case"` rejects unknown
    // enum variants. The process must NOT keep running silently with a
    // surprise fallback — that's exactly the silent-misconfig bug we
    // want to catch in CI long before it reaches prod.
    let (exited_ok, all) = spawn_and_wait_for_exit(
        &[("AI_MEMORY_AUTO_SCOPE__MODE", "per_universe")],
        Duration::from_secs(5),
    );
    assert!(
        !exited_ok,
        "invalid `mode` must NOT result in a successful startup.\nstderr:\n{all}"
    );
}

// ─── Boundary-value cases for the numeric fields ──────────────────────
//
// figment coerces strings → integers and the `PerActorMap` constructor
// gracefully clamps the degenerate-zero cases so an operator who copy-
// pastes a placeholder doesn't get a server that immediately evicts
// every entry (`ttl=0`) or refuses to store anything (`max_entries=0`).
// These pin that behaviour so a future refactor either keeps the clamp
// or makes the failure loud.

#[test]
fn max_entries_zero_clamps_and_starts() {
    // `0` is a footgun — without clamping, every insertion would be
    // immediately evicted and per_actor would silently degrade to the
    // single slot. `PerActorMap::new` clamps to 1; the engine must
    // still start.
    let (line, all) = spawn_and_wait_for_log(
        &[
            ("AI_MEMORY_AUTO_SCOPE__MODE", "per_session"),
            ("AI_MEMORY_AUTO_SCOPE__MAX_ENTRIES", "0"),
        ],
        STARTUP_NEEDLE,
        STARTUP_TIMEOUT,
    );
    let line = line.unwrap_or_else(|| {
        panic!("startup log line not found with max_entries=0.\nstderr:\n{all}")
    });
    // The startup log shows the configured value (0), not the
    // post-clamp internal value — that's a separate observability
    // concern. Asserting `PerSession` is enough proof that startup
    // succeeded; the clamp itself is exercised by the unit tests on
    // `PerActorMap::new`.
    assert!(
        line.contains("PerSession"),
        "max_entries=0 should still start in per_session mode: {line}\nfull stderr:\n{all}"
    );
}

#[test]
fn session_ttl_zero_clamps_and_starts() {
    // Same logic as max_entries=0: zero TTL = entries expire on
    // insertion = per_actor silently degrades. `PerActorMap::new` falls
    // back to DEFAULT_PER_KEY_TTL when ttl.is_zero(). Engine must
    // still start.
    let (line, all) = spawn_and_wait_for_log(
        &[
            ("AI_MEMORY_AUTO_SCOPE__MODE", "per_actor"),
            ("AI_MEMORY_AUTO_SCOPE__SESSION_TTL_SECS", "0"),
        ],
        STARTUP_NEEDLE,
        STARTUP_TIMEOUT,
    );
    let line =
        line.unwrap_or_else(|| panic!("startup log line not found with ttl=0.\nstderr:\n{all}"));
    assert!(
        line.contains("PerActor"),
        "session_ttl_secs=0 should still start in per_actor mode: {line}\nfull stderr:\n{all}"
    );
}

#[test]
fn empty_mode_string_fails_fast() {
    // `""` is not a valid serde enum variant. Just like
    // `per_universe`, this must abort startup — never quietly default
    // to single.
    let (exited_ok, all) = spawn_and_wait_for_exit(
        &[("AI_MEMORY_AUTO_SCOPE__MODE", "")],
        Duration::from_secs(5),
    );
    assert!(
        !exited_ok,
        "empty `mode` must NOT result in a successful startup.\nstderr:\n{all}"
    );
}

#[test]
fn pascalcase_mode_fails_fast() {
    // The serde rename is `snake_case`. `PerSession` (the rust enum
    // variant name) is NOT a valid wire value; the operator must use
    // `per_session`. Pin this so a refactor doesn't quietly accept
    // both forms (which would mask a future case-sensitivity bug).
    let (exited_ok, all) = spawn_and_wait_for_exit(
        &[("AI_MEMORY_AUTO_SCOPE__MODE", "PerSession")],
        Duration::from_secs(5),
    );
    assert!(
        !exited_ok,
        "PascalCase `mode` must NOT result in a successful startup.\nstderr:\n{all}"
    );
}
