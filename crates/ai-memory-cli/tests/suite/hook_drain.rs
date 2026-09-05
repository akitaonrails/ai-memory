//! Smoke tests for the hidden hook spool drainer command.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

#[test]
fn hidden_hook_drain_empty_spool_writes_no_stdout() {
    let tmp = tempfile::tempdir().expect("tempdir");

    let output = Command::new(bin())
        .args(["--data-dir"])
        .arg(tmp.path())
        .arg("hook-drain")
        .output()
        .expect("run hook-drain");

    assert!(output.status.success());
    assert!(output.stdout.is_empty());
}

#[test]
fn hook_drain_is_hidden_from_top_level_help() {
    let output = Command::new(bin())
        .arg("--help")
        .output()
        .expect("run help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("help utf8");
    assert!(!stdout.contains("hook-drain"));
}
