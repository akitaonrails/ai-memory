//! Shared spawned-server helpers for the CLI end-to-end suites.
//!
//! `backfill_e2e`, `doctor_e2e`, and `message_e2e` each drive the shipped
//! `ai-memory` binary against a real `ai-memory serve` child, exactly as an
//! operator would. These are the pieces they all share: a hermetic `Command`,
//! the server start and lifetime guard, ephemeral port allocation, the
//! `by-agent` session count, and the subcommand runner.
//!
//! Kept deliberately cfg-agnostic (no `#![cfg(unix)]`) so the non-unix
//! `message_e2e` can use it too. The genuinely POSIX-specific fixtures — the
//! native `~/.claude/projects/<enc-cwd>/` transcript layout — stay in the
//! individual `#[cfg(unix)]` files.

use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

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

/// A free loopback port. It is released before the caller binds it; a spawned
/// server goes through [`start_serve`], which survives losing that race.
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// What `serve` logs once its HTTP listener is bound, with the address.
const SERVE_READY: &str = "MCP HTTP server ready";

/// Start `ai-memory serve` on a free loopback port and wait until it answers
/// HTTP. `command` builds the configured serve command for a port; the
/// child's stderr goes to `log`, with the serve module's log turned on.
///
/// [`free_port`] releases the port before the server binds it, and under a
/// parallel test load another socket can take it in between. The server then
/// exits at once and a bare readiness loop only times out, hiding why; and
/// another test's server there would answer the probe. So the child must log
/// that it bound the port before an answer counts. A child that failed to
/// bind is restarted on a fresh port; any other exit, or no answer within the
/// deadline, panics with the child's stderr.
pub async fn start_serve(
    client: &reqwest::Client,
    log: &Path,
    command: impl Fn(u16) -> Command,
) -> (ServerGuard, String) {
    start_serve_on(client, log, command, free_port).await
}

async fn start_serve_on(
    client: &reqwest::Client,
    log: &Path,
    command: impl Fn(u16) -> Command,
    mut next_port: impl FnMut() -> u16,
) -> (ServerGuard, String) {
    const ATTEMPTS: usize = 5;
    let output = || fs::read_to_string(log).unwrap_or_default();
    for _ in 0..ATTEMPTS {
        let port = next_port();
        let addr = format!("127.0.0.1:{port}");
        let base = format!("http://{addr}");
        let stderr = fs::File::create(log).expect("create serve log");
        let mut server = ServerGuard(
            command(port)
                .env("RUST_LOG", "ai_memory_cli::commands::serve=info")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(stderr)
                .spawn()
                .expect("spawn serve"),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let bound = output()
                .lines()
                .any(|line| line.contains(SERVE_READY) && line.contains(&addr));
            // Bounded so a stalled answer cannot hide the child's exit.
            let answered = bound
                && client
                    .get(format!("{base}/mcp"))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                    .is_ok();
            match server.0.try_wait().expect("poll serve") {
                None if answered => return (server, base),
                None => {}
                // `serve` reports a failed bind as `binding <addr>`.
                Some(_) if output().contains(&format!("binding {addr}")) => break,
                Some(status) => panic!("serve exited ({status}) before answering:\n{}", output()),
            }
            assert!(
                Instant::now() < deadline,
                "serve never answered on {base}:\n{}",
                output()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    panic!("serve could not bind a free port in {ATTEMPTS} attempts");
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

mod slow {
    use super::*;

    fn serve(root: &Path, port: u16) -> Command {
        let mut cmd = hermetic(BIN);
        cmd.env("HOME", root)
            .env("USERPROFILE", root)
            .env("AI_MEMORY_HOME", root)
            .env("AI_MEMORY_DATA_DIR", root.join("data"))
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
            .env("AI_MEMORY_BACKFILL_ON_START", "false")
            .env("AI_MEMORY_AUTO_IMPROVE__SCHEDULER__ENABLED", "false")
            .env("RUST_LOG", "off")
            .current_dir(root)
            .args(["serve", "--transport", "http", "--bind"])
            .arg(format!("127.0.0.1:{port}"))
            .arg("--no-watcher");
        cmd
    }

    /// A port taken between [`free_port`] and the server's bind makes the
    /// server exit at once; the start moves to a fresh port instead of
    /// waiting out the readiness deadline, and does not take the answer of
    /// whatever holds the port for its own.
    #[tokio::test]
    async fn start_serve_moves_off_a_port_taken_before_the_bind() {
        use std::io::{Read as _, Write as _};

        let root = tempfile::tempdir().unwrap().keep();
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy = taken.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in taken.incoming().flatten() {
                let _ = stream.read(&mut [0; 1024]);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            }
        });
        let mut ports = [busy].into_iter();
        let client = reqwest::Client::new();
        let started = Instant::now();
        let (_server, base) = start_serve_on(
            &client,
            &root.join("serve.log"),
            |port| serve(&root, port),
            || ports.next().unwrap_or_else(free_port),
        )
        .await;
        assert_ne!(base, format!("http://127.0.0.1:{busy}"));
        assert!(started.elapsed() < Duration::from_secs(20));
    }

    /// Any other early exit fails at once with the server's own error.
    #[tokio::test]
    #[should_panic(expected = "serve exited")]
    async fn start_serve_reports_an_early_exit() {
        let root = tempfile::tempdir().unwrap().keep();
        start_serve(&reqwest::Client::new(), &root.join("serve.log"), |port| {
            let mut cmd = serve(&root, port);
            cmd.arg("--no-such-flag");
            cmd
        })
        .await;
    }
}
