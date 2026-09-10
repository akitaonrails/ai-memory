//! Subprocess smoke tests for signal shutdown of `ai-memory serve` (#699).
//!
//! Both transports used to be unstoppable in their own way: stdio listened
//! for no signal at all, and http listened only for SIGINT — so SIGTERM,
//! what `docker stop` and `systemctl stop` send, reached no handler. As PID 1
//! in the container the kernel discarded it outright and `docker stop` waited
//! out its whole grace period before SIGKILL; anywhere else it fell through
//! to the kernel's default disposition and killed the process instantly, with
//! no drain. Only a real spawned process can prove the difference: signal
//! disposition is a property of the process, not of any function these tests
//! could call in-crate.
//!
//! Each test spawns the built binary against a temp data dir, waits for the
//! transport's own ready line (so a child that died during boot can never
//! pass vacuously), sends the signal with `kill`, and requires the child to
//! be gone inside a deadline. `kill` is shelled out to deliberately: sending
//! a signal from Rust needs either `libc` (a new dependency) or `unsafe`
//! (forbidden by the workspace lint). The third test completes the MCP
//! `initialize` handshake before signalling, because the stdio arm takes a
//! different branch before and after one.
//!
//! The exit status is the load-bearing half of each assertion. A test child is
//! not PID 1, so an unhandled signal still ends it — just by the kernel's
//! default disposition, which reports as "killed by signal N" rather than a
//! clean code. Requiring `success()` is therefore what separates a handled
//! shutdown from no handler at all; the deadline covers the other failure
//! shape, a handler that runs but leaves the process alive.

#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Boot does migrations, the pre-migration archive and index work; the
/// autoscope suite budgets 8s for the same startup, and this one runs three
/// servers in parallel with the rest of the suite. Widened from 20s after one
/// loaded machine spent 11s of wall clock on what these tests normally finish
/// in about 1s: a 10x resource-pressure factor, not a logic race. Being
/// generous is free, because elapsed time is not what any test here
/// discriminates on — see [`EXIT_TIMEOUT`].
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// The shutdown itself is bounded by `SHUTDOWN_GRACE` (5s) in `serve.rs`.
/// Anything past that plus process teardown means the signal was ignored,
/// which is the bug.
///
/// 30s is 6x that bound, and widening it from 10s weakens no assertion: the
/// discriminator is the exit status, not the clock. If the handler regresses
/// away, the child falls back to the kernel's default disposition, dies at
/// once, and fails `status.success()` immediately however long this deadline
/// is. The deadline covers only the other failure shape — a handler that runs
/// but leaves the process alive, #699's PID-1 symptom — and any value past
/// the grace period catches that one. A tight deadline therefore buys no
/// sensitivity and costs flakiness on a loaded runner.
///
/// The ceiling for both constants is nextest's everyday profile, which kills
/// a test at 120s (`slow-timeout` 5s x 24): 60 + 10 + 30 keeps even the
/// handshake test's worst case under it, so a wedged child still fails with
/// this file's own diagnostic instead of a bare timeout. The tests stay in
/// that tier — they cost about 0.25s when nothing is wrong.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The handshake is one round trip over a pipe against an already-booted
/// server; this is slack for a loaded box, not a real budget.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

const HTTP_READY: &str = "MCP HTTP server ready";
const STDIO_READY: &str = "MCP server ready on stdio";
/// The stdio arm's post-handshake shutdown log — the branch only
/// [`stdio_server_exits_on_sigterm_after_initialize`] reaches.
const STDIO_STOP_AFTER_HANDSHAKE: &str = "stopping the stdio transport";
/// And the pre-handshake one, from the select that runs before a client
/// connects. Distinguishing the two is what keeps that test honest.
const STDIO_STOP_BEFORE_CLIENT: &str = "shutdown signal received before a client connected";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

/// Pump one of the child's output streams into a channel from a background
/// thread, so waiting for a line has a wall-clock timeout rather than blocking
/// on a read syscall. The handle yields everything read, for failure messages.
fn pump_lines<R>(reader: R, tx: mpsc::Sender<String>) -> JoinHandle<String>
where
    R: std::io::Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut all = String::new();
        for line in BufReader::new(reader).lines() {
            let Ok(line) = line else { break };
            all.push_str(&line);
            all.push('\n');
            if tx.send(line).is_err() {
                break;
            }
        }
        all
    })
}

/// A spawned `ai-memory serve` whose stderr is pumped into a channel by a
/// background thread, so waiting for a log line has a wall-clock timeout
/// rather than blocking on a read syscall.
struct Server {
    child: Child,
    lines: Receiver<String>,
    pump: Option<JoinHandle<String>>,
    /// Held open for stdio: closing it is EOF, which stops the transport on
    /// its own and would let the test pass without any signal being handled.
    stdin: Option<ChildStdin>,
    /// JSON-RPC frames, present only when stdout was piped: the handshake is
    /// the one thing here that needs to read the transport itself.
    frames: Option<Receiver<String>>,
    seen: String,
    _data_dir: TempDir,
}

impl Server {
    /// stdout at `/dev/null`: a test that only signals never reads it.
    fn spawn(transport: &str) -> Self {
        Self::spawn_inner(transport, Stdio::null())
    }

    /// stdout piped, so [`Server::initialize`] can read the response frame.
    fn spawn_with_stdout(transport: &str) -> Self {
        Self::spawn_inner(transport, Stdio::piped())
    }

    fn spawn_inner(transport: &str, stdout: Stdio) -> Self {
        let data_dir = TempDir::new().expect("tempdir for serve");
        let mut cmd = Command::new(bin());
        cmd.args(["serve", "--transport", transport]);
        if transport == "http" {
            // Any free port: nothing here talks to the listener, it only has
            // to exist so the server reaches its ready line.
            cmd.args(["--bind", "127.0.0.1:0"]);
        }
        cmd.arg("--data-dir")
            .arg(data_dir.path())
            .env("AI_MEMORY_DATA_DIR", data_dir.path())
            // Hermetic: the default provider would start a model download in
            // the background of every spawned server.
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(stdout)
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn ai-memory serve");

        let stderr = child.stderr.take().expect("stderr piped");
        let stdin = child.stdin.take();
        let (tx, lines) = mpsc::channel::<String>();
        let pump = pump_lines(stderr, tx);
        let frames = child.stdout.take().map(|stdout| {
            let (tx, frames) = mpsc::channel::<String>();
            // Detached: only stderr's accumulated copy is used, and the thread
            // ends at EOF when the child exits.
            drop(pump_lines(stdout, tx));
            frames
        });

        Self {
            child,
            lines,
            pump: Some(pump),
            stdin,
            frames,
            seen: String::new(),
            _data_dir: data_dir,
        }
    }

    /// Drive the MCP `initialize` handshake to completion, returning the
    /// server's response frame.
    ///
    /// `serve` resolves only once a client has initialized, so this is what
    /// moves the stdio arm out of its pre-handshake select and into the branch
    /// that owns the service — where every real harness ends up.
    fn initialize(&mut self, timeout: Duration) -> String {
        // `2025-11-25` is `ProtocolVersion::LATEST` in rmcp 1.7 (its
        // `model.rs`); the server answers with its own version when the
        // client's is newer, so this only has to parse. `capabilities` and
        // `clientInfo` are the request's required fields.
        const INITIALIZE: &str = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"#,
            r#""protocolVersion":"2025-11-25","capabilities":{},"#,
            r#""clientInfo":{"name":"shutdown-signals-test","version":"0"}}}"#,
            "\n",
        );
        let stdin = self.stdin.as_mut().expect("stdin piped");
        stdin
            .write_all(INITIALIZE.as_bytes())
            .expect("write the initialize request");
        stdin.flush().expect("flush the initialize request");
        let response = self
            .frames
            .as_ref()
            .expect("stdout piped")
            .recv_timeout(timeout);
        match response {
            Ok(frame) => frame,
            // Disconnected also lands here: stdout closed means the child died.
            Err(_) => panic!(
                "no initialize response on stdout within {timeout:?}.\nstderr:\n{}",
                self.stderr()
            ),
        }
    }

    /// Block until a stderr line contains `needle`, or the timeout expires.
    fn wait_for(&mut self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    let hit = line.contains(needle);
                    self.seen.push_str(&line);
                    self.seen.push('\n');
                    if hit {
                        return true;
                    }
                }
                // Disconnected means stderr closed: the child died during boot.
                Err(_) => return false,
            }
        }
    }

    fn signal(&self, name: &str) {
        let pid = self.child.id().to_string();
        let status = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(&pid)
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{name} {pid} failed: {status}");
    }

    /// Poll for the child's exit, returning `None` if it outlives `timeout`.
    fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => panic!("try_wait on the serve child failed: {error}"),
            }
        }
    }

    /// Everything the child wrote to stderr, for a failure message. Kills the
    /// child first, so the pump thread's read reaches EOF; call it last.
    fn stderr(&mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stdin.take();
        match self.pump.take() {
            Some(pump) => pump.join().unwrap_or_else(|_| self.seen.clone()),
            None => self.seen.clone(),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // A panicking test must not leave a server holding the temp data
        // dir's serve lock.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `docker stop` and `systemctl stop` both send SIGTERM, which the http arm
/// never listened for: it handled ctrl-c only, so a stop either burned the
/// full grace period and ended in SIGKILL (as PID 1, where the kernel
/// discards an unhandled signal) or killed the server outright, undrained
/// (#699).
#[test]
fn http_server_exits_on_sigterm() {
    let mut server = Server::spawn("http");
    if !server.wait_for(HTTP_READY, READY_TIMEOUT) {
        panic!(
            "http server never logged {HTTP_READY:?} within {READY_TIMEOUT:?}; \
             the signal was never sent.\nstderr:\n{}",
            server.stderr()
        );
    }

    server.signal("TERM");

    let Some(status) = server.wait_for_exit(EXIT_TIMEOUT) else {
        panic!(
            "http server still running {EXIT_TIMEOUT:?} after SIGTERM.\nstderr:\n{}",
            server.stderr()
        );
    };
    assert!(
        status.success(),
        "SIGTERM is a normal stop, so the exit must be clean. Got {status}.\nstderr:\n{}",
        server.stderr()
    );
}

/// The stdio arm awaited the transport and nothing else, so ctrl-c could not
/// stop it at all — the reported symptom of #699.
#[test]
fn stdio_server_exits_on_sigint() {
    let mut server = Server::spawn("stdio");
    if !server.wait_for(STDIO_READY, READY_TIMEOUT) {
        panic!(
            "stdio server never logged {STDIO_READY:?} within {READY_TIMEOUT:?}; \
             the signal was never sent.\nstderr:\n{}",
            server.stderr()
        );
    }
    // Precondition for a non-vacuous result: stdin is still open, so the
    // transport has no reason of its own to stop.
    assert!(
        server.stdin.is_some(),
        "stdin must stay piped and open, or EOF — not the signal — ends the transport"
    );

    server.signal("INT");

    let Some(status) = server.wait_for_exit(EXIT_TIMEOUT) else {
        panic!(
            "stdio server still running {EXIT_TIMEOUT:?} after SIGINT.\nstderr:\n{}",
            server.stderr()
        );
    };
    assert!(
        status.success(),
        "a signal-triggered stop is a normal shutdown. Got {status}.\nstderr:\n{}",
        server.stderr()
    );
}

/// Both tests above signal before any client connects, so on stdio they
/// resolve in the pre-handshake select and return early. A real harness
/// completes the MCP `initialize` handshake first, which lands the arm in the
/// branch that takes the service's cancellation token, cancels it and waits
/// under the grace period — the code that actually runs in production, and
/// which nothing else here reaches (#699).
#[test]
fn stdio_server_exits_on_sigterm_after_initialize() {
    let mut server = Server::spawn_with_stdout("stdio");
    if !server.wait_for(STDIO_READY, READY_TIMEOUT) {
        panic!(
            "stdio server never logged {STDIO_READY:?} within {READY_TIMEOUT:?}; \
             the handshake was never started.\nstderr:\n{}",
            server.stderr()
        );
    }

    // Precondition for a non-vacuous result: until the handshake completes,
    // the signal lands in the pre-handshake select the other tests cover.
    let response = server.initialize(HANDSHAKE_TIMEOUT);
    assert!(
        response.contains(r#""result""#) && response.contains("serverInfo"),
        "the initialize response must carry a result, or the handshake did not \
         complete: {response}.\nstderr:\n{}",
        server.stderr()
    );

    server.signal("TERM");

    let Some(status) = server.wait_for_exit(EXIT_TIMEOUT) else {
        panic!(
            "stdio server still running {EXIT_TIMEOUT:?} after SIGTERM.\nstderr:\n{}",
            server.stderr()
        );
    };
    let stderr = server.stderr();
    assert!(
        status.success(),
        "a signal-triggered stop is a normal shutdown. Got {status}.\nstderr:\n{stderr}"
    );
    // Which line was logged says which branch ran, so this test cannot drift
    // back into covering the early path it exists to complement.
    assert!(
        stderr.contains(STDIO_STOP_AFTER_HANDSHAKE),
        "expected the post-handshake shutdown log {STDIO_STOP_AFTER_HANDSHAKE:?}.\
         \nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains(STDIO_STOP_BEFORE_CLIENT),
        "the pre-handshake branch handled the signal, so this test covers the \
         same path as the others.\nstderr:\n{stderr}"
    );
}
