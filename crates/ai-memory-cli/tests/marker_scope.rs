//! Subprocess tests for `.ai-memory.toml` scope resolution in client commands.
//!
//! The unit tests in `commands::tests` cover the precedence table directly.
//! What needs a real process is the env-var wiring — `AI_MEMORY_IGNORE_MARKER`
//! is read from the process environment, and `$HOME` bounds the marker walk,
//! so both have to be set before the binary starts.
//!
//! Each test runs a command that resolves its scope and then fails to reach a
//! server. That is deliberate: resolution happens first, so the stderr notice
//! is emitted regardless, and the test never needs a live engine.

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

/// The tempdir's real path. `TempDir` hands back `/var/folders/…` on macOS
/// while the child's `getcwd()` reports the resolved `/private/var/folders/…`
/// — without canonicalising, `$HOME` never equals any directory the walk
/// visits and the marker search escapes all the way to `/`.
fn real_path(dir: &TempDir) -> PathBuf {
    dir.path().canonicalize().expect("canonicalising tempdir")
}

/// Run `search` from `cwd` with a deliberately dead server and return stderr.
///
/// `$HOME` is pinned to `cwd` so the marker walk cannot reach the developer's
/// real `~/.ai-memory.toml`, and the data dir is redirected for the same
/// reason — neither test should read or write the machine's actual install.
fn search_stderr(cwd: &Path, data_dir: &Path, extra_env: &[(&str, &str)], args: &[&str]) -> String {
    let mut cmd = Command::new(bin());
    cmd.arg("search")
        .arg("anything")
        .args(args)
        .current_dir(cwd)
        .env("HOME", cwd)
        .env("AI_MEMORY_DATA_DIR", data_dir)
        // Port 1 is reserved and never listening: the command resolves its
        // scope, prints the notice, then fails on connect.
        .env("AI_MEMORY_SERVER_URL", "http://127.0.0.1:1")
        // `AI_MEMORY_HOME` outranks `$HOME` in `path_util::home_dir`, so an
        // exported one on the developer's machine would unpin the walk.
        .env_remove("AI_MEMORY_HOME")
        .env_remove("AI_MEMORY_HOST_CWD")
        .env_remove("AI_MEMORY_SCOPE_CWD")
        .env_remove("AI_MEMORY_IGNORE_MARKER")
        .env_remove("AI_MEMORY_PROJECT_STRATEGY");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("running ai-memory search");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn marker_tree(body: &str) -> (TempDir, TempDir) {
    let cwd = TempDir::new().expect("tempdir for cwd");
    let data = TempDir::new().expect("tempdir for data dir");
    std::fs::write(cwd.path().join(".ai-memory.toml"), body).expect("writing marker");
    (cwd, data)
}

#[test]
fn marker_workspace_is_used_and_announced() {
    let (cwd, data) = marker_tree("workspace = \"acme\"\n");

    let stderr = search_stderr(&real_path(&cwd), &real_path(&data), &[], &[]);

    assert!(
        stderr.contains("scope acme/"),
        "the resolved scope must come from the marker and be announced: {stderr}"
    );
    assert!(
        stderr.contains(".ai-memory.toml"),
        "the notice names the marker that decided it: {stderr}"
    );
    assert!(
        stderr.contains("workspace + project from"),
        "a workspace-only marker also selects hook-compatible basename(cwd): {stderr}"
    );
}

#[test]
fn ignore_marker_env_restores_the_default_workspace() {
    let (cwd, data) = marker_tree("workspace = \"acme\"\n");

    let stderr = search_stderr(
        &real_path(&cwd),
        &real_path(&data),
        &[("AI_MEMORY_IGNORE_MARKER", "1")],
        &[],
    );

    assert!(
        !stderr.contains("scope acme/"),
        "AI_MEMORY_IGNORE_MARKER=1 must skip the marker entirely: {stderr}"
    );
}

#[test]
fn explicit_workspace_flag_beats_the_marker() {
    let (cwd, data) = marker_tree("workspace = \"acme\"\n");

    let stderr = search_stderr(
        &real_path(&cwd),
        &real_path(&data),
        &[],
        &["--workspace", "default"],
    );

    assert!(
        !stderr.contains("scope acme/"),
        "an explicit --workspace wins, so no marker notice: {stderr}"
    );
}

#[test]
fn a_tree_without_a_marker_is_unchanged() {
    let cwd = TempDir::new().expect("tempdir for cwd");
    let data = TempDir::new().expect("tempdir for data dir");

    let stderr = search_stderr(&real_path(&cwd), &real_path(&data), &[], &[]);

    assert!(
        !stderr.contains("ai-memory: scope "),
        "no marker means no scope notice and no behaviour change: {stderr}"
    );
}

#[test]
fn mounted_scope_cwd_reads_marker_while_host_cwd_keeps_project_identity() {
    let root = TempDir::new().expect("scope root");
    let data = TempDir::new().expect("data dir");
    std::fs::write(
        root.path().join(".ai-memory.toml"),
        "workspace = \"acme\"\n",
    )
    .unwrap();
    let mounted_cwd = root.path().join("crates/cli");
    std::fs::create_dir_all(&mounted_cwd).unwrap();
    let mounted_cwd = mounted_cwd.canonicalize().unwrap();
    let root = real_path(&root);
    let data = real_path(&data);
    let stderr = search_stderr(
        &mounted_cwd,
        &data,
        &[
            ("AI_MEMORY_HOST_CWD", "/host/repo/crates/cli"),
            ("AI_MEMORY_SCOPE_CWD", mounted_cwd.to_str().unwrap()),
            ("HOME", root.to_str().unwrap()),
        ],
        &[],
    );

    assert!(
        stderr.contains("scope acme/cli"),
        "lookup must use the mounted path and naming must use host cwd: {stderr}"
    );
}
