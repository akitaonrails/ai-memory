//! GHSA-6pmr-q79m-87vp items 1 and 2, through the shipped binary: the
//! print-only previews and the Hermes/Pool `--apply` block are written to the
//! terminal, so the inline bearer must be masked there. These exercise an
//! agent that renders its block to stdout (Hermes) and `setup-agent`, where the
//! whole command only ever prints a snippet.

#![cfg(unix)]

use std::path::Path;
use std::process::Output;

use crate::e2e_support::hermetic;

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
const TOKEN: &str = "aim_SYNTHETICtestvalueAAAAAAAAAAAAAAAAAAAAAAA";

fn run(home: &Path, args: &[&str]) -> Output {
    hermetic(BIN)
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .unwrap()
}

/// A Hermes preview (`install-hooks`, no `--apply`) prints its ready-to-paste
/// YAML block to stdout; the inline bearer is masked there.
#[test]
fn install_hooks_hermes_preview_masks_the_inline_bearer() {
    let home = tempfile::tempdir().unwrap();
    let out = run(
        home.path(),
        &[
            "install-hooks",
            "--agent",
            "hermes",
            "--server-url",
            "http://127.0.0.1:49374",
            "--auth-token",
            TOKEN,
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains(TOKEN),
        "the preview leaked the token: {stdout}"
    );
    assert!(stdout.contains("[REDACTED]"), "not masked: {stdout}");
}

/// Hermes on `--apply` writes no file — it prints the ready-to-paste block.
/// When the auth-token store fails (the #552 inline fallback, forced here with a
/// read-only data dir), the bearer would otherwise be embedded verbatim in that
/// printed block; item 2 masks it there too.
#[test]
fn install_hooks_hermes_apply_masks_the_inline_bearer_on_store_failure() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    // A read-only data dir makes `store_hook_auth_token` fail, so install-hooks
    // falls back to embedding the credential inline instead of persisting it —
    // the exact path item 2 guards.
    let data = home.path().join("ro-data");
    std::fs::create_dir(&data).unwrap();
    let mut perms = std::fs::metadata(&data).unwrap().permissions();
    perms.set_mode(0o500);
    std::fs::set_permissions(&data, perms).unwrap();
    let out = run(
        home.path(),
        &[
            "install-hooks",
            "--agent",
            "hermes",
            "--apply",
            "--data-dir",
            data.to_str().unwrap(),
            "--server-url",
            "http://127.0.0.1:49374",
            "--auth-token",
            TOKEN,
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !stdout.contains(TOKEN),
        "the apply fallback leaked the token: {stdout}"
    );
    assert!(
        stdout.contains("[REDACTED]"),
        "the apply fallback did not mask the bearer: {stdout}"
    );
}

/// `setup-agent` only ever prints a snippet to merge; the bearer is masked.
#[test]
fn setup_agent_hermes_masks_the_inline_bearer() {
    let home = tempfile::tempdir().unwrap();
    let to = home.path().join("stage");
    let out = run(
        home.path(),
        &[
            "setup-agent",
            "--agent",
            "hermes",
            "--to",
            to.to_str().unwrap(),
            "--server-url",
            "http://127.0.0.1:49374",
            "--auth-token",
            TOKEN,
        ],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!stdout.contains(TOKEN), "setup-agent leaked: {stdout}");
    assert!(stdout.contains("[REDACTED]"), "not masked: {stdout}");
}
