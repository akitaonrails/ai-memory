//! Shared spawned-server helpers for the CLI end-to-end suites.
//!
//! `backfill_e2e`, `doctor_e2e`, and `message_e2e` each drive the shipped
//! `ai-memory` binary against a real `ai-memory serve` child, exactly as an
//! operator would. These are the pieces they all share: a hermetic `Command`,
//! the server-lifetime guard, ephemeral port allocation, the `by-agent`
//! session count, and the subcommand runner.
//!
//! Kept deliberately cfg-agnostic (no `#![cfg(unix)]`) so the non-unix
//! `message_e2e` can use it too. The genuinely POSIX-specific fixtures — the
//! native `~/.claude/projects/<enc-cwd>/` transcript layout — stay in the
//! individual `#[cfg(unix)]` files.

use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");

/// Start from a clean, hermetic environment: drop every ambient `AI_MEMORY_*`
/// var (a developer box or this project's own MCP config may export
/// `AI_MEMORY_AUTH_TOKEN`, `AI_MEMORY_SERVER_URL`, scope names, …) so the child
/// sees only what the test sets. Without this the spawned server would inherit
/// an auth token and reject the test's own requests.
pub fn hermetic(program: &str) -> Command {
    let mut cmd = Command::new(program);
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("AI_MEMORY_") {
            cmd.env_remove(key);
        }
    }
    cmd
}

/// Kill the spawned server when the test ends, pass or fail.
pub struct ServerGuard(pub Child);
impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A free loopback port. The brief unbind→rebind race is acceptable for a
/// slow-tier test and is the same approach the shell smoke test uses.
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Write the JSONL lines (each already a `Value`) as one transcript file.
///
/// Only the POSIX-gated fixtures plant transcripts, so this is unused on
/// non-unix targets where those modules are compiled out.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn write_jsonl(path: &Path, lines: &[Value]) {
    let mut body = String::new();
    for line in lines {
        body.push_str(&line.to_string());
        body.push('\n');
    }
    fs::write(path, body).expect("write transcript");
}

/// Sum the server's per-agent session counts for the scope. A 404 (the
/// no-create scope lookup for a project that has never been written to) counts
/// as zero — the pre-import / pre-existence state.
pub async fn session_count(
    client: &reqwest::Client,
    base: &str,
    workspace: &str,
    project: &str,
) -> u64 {
    let resp = client
        .get(format!("{base}/admin/sessions/by-agent"))
        .query(&[("workspace", workspace), ("project", project)])
        .send()
        .await
        .expect("by-agent request");
    if !resp.status().is_success() {
        return 0;
    }
    let body: Value = resp.json().await.expect("by-agent json");
    body["by_agent"]
        .as_array()
        .map(|agents| {
            agents
                .iter()
                .filter_map(|a| a["sessions"].as_u64())
                .sum::<u64>()
        })
        .unwrap_or(0)
}

/// Run a subcommand of the built binary to completion, returning its stdout.
/// The scope/server/home environment is shared with the spawned server so the
/// client talks to the same store the way a real install does. Pass `cwd` when
/// the subcommand resolves the current project from its working directory
/// (backfill/doctor); pass `None` to inherit the test's own cwd (message).
pub fn run_cli(
    args: &[&str],
    data_dir: &Path,
    home: &Path,
    cwd: Option<&Path>,
    base: &str,
) -> String {
    let mut cmd = hermetic(BIN);
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let out = cmd
        .env("AI_MEMORY_DATA_DIR", data_dir)
        .env("AI_MEMORY_HOME", home)
        .env("AI_MEMORY_SERVER_URL", base)
        .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
        .env("RUST_LOG", "off")
        .output()
        .unwrap_or_else(|e| panic!("spawn `{}`: {e}", args.join(" ")));
    assert!(
        out.status.success(),
        "`{}` failed: {}\nstdout: {}\nstderr: {}",
        args.join(" "),
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8(out.stdout).expect("stdout utf8")
}
