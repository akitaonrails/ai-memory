//! `serve --transport stdio` must stop when the operator interrupts it (#699).

#![cfg(unix)]

/// Startup, one interrupt and one shutdown: seconds, not milliseconds.
mod slow {
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
    /// Logged by `start_watcher` when `serve --no-watcher` is honoured.
    const WATCHER_DISABLED: &str = "watcher disabled by --no-watcher";
    /// Logged by `start_watcher` when it actually installs an FSEvents/inotify
    /// instance. Must not appear here: this test does not exercise watching.
    const WATCHER_STARTED: &str = "starting wiki watcher";

    fn wait_for_exit(child: &mut Child, budget: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            match child.try_wait().expect("try_wait") {
                Some(status) => return Some(status),
                None => thread::sleep(Duration::from_millis(100)),
            }
        }
        None
    }

    /// A stdio server must shut down when it is interrupted, even when the
    /// disposition it inherited would discard the signal.
    ///
    /// What this covers: that `serve` installs a signal handler instead of
    /// relying on the default disposition, that it does so *before* the MCP
    /// `initialize` handshake (a server started by hand has no client writing
    /// to stdin and never gets past `serve()`), and that observing the
    /// interrupt actually ends the process rather than parking on runtime
    /// shutdown behind the uncancellable blocking read of stdin.
    ///
    /// What it does not cover: the reported environment itself. #699 is the
    /// container entrypoint running this binary as PID 1, where the kernel
    /// drops a default-disposition signal sent to a namespace's init. A test
    /// cannot portably become PID 1, so the child inherits `SIG_IGN` for
    /// SIGINT instead — `trap "" INT` before `exec`, which survives the exec.
    /// The mechanism that swallows the signal differs; the property under test
    /// is the same one that fixes both, and reverting the fix fails this test.
    #[test]
    fn stdio_serve_exits_when_interrupted() {
        let data_dir = tempfile::TempDir::new().expect("tempdir");

        let mut child = Command::new("sh")
            .arg("-c")
            // Explicit transport: the default is stdio today, and a test that
            // rides that default would quietly start testing something else if
            // it ever changed. `--no-watcher`: this test does not watch the
            // wiki; concurrent serve children can exhaust the machine-global
            // FSEvents/inotify instance (#745).
            .arg(r#"trap "" INT; exec "$1" serve --transport stdio --no-watcher"#)
            .arg("sh")
            .arg(BIN)
            .env("AI_MEMORY_DATA_DIR", data_dir.path())
            // Hermetic, the way the other two server-spawning suites already
            // are: the 2.0 embedder default would start a background model
            // download on this server, and an ambient RUST_LOG below info
            // would delete the very line this test waits a minute for.
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
            .env("RUST_LOG", "info")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn serve");

        // Hold stdin open: closing it is the other way this server stops, and
        // it would hide whether the interrupt was observed at all.
        let _stdin = child.stdin.take().expect("stdin");

        let stderr = child.stderr.take().expect("stderr");
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let ready = line.contains("ready on stdio");
                let _ = tx.send(line);
                if ready {
                    break;
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(60);
        let mut ready = false;
        let mut startup = String::new();
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(line) => {
                    startup.push_str(&line);
                    startup.push('\n');
                    if line.contains("ready on stdio") {
                        ready = true;
                        break;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(ready, "server never reported it was ready on stdio");
        // Ready is logged after the watcher decision, so this buffer
        // includes that decision. None of this test watches the wiki;
        // the watcher is a machine-global FSEvents/inotify instance
        // concurrent children can exhaust (#745).
        assert!(
            startup.contains(WATCHER_DISABLED),
            "spawned serve must log that the watcher was opted out.\nstderr:\n{startup}"
        );
        assert!(
            !startup.contains(WATCHER_STARTED),
            "spawned serve must not install a wiki watcher.\nstderr:\n{startup}"
        );

        let killed = Command::new("kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("send SIGINT");
        assert!(killed.success(), "kill -INT failed");

        let status = wait_for_exit(&mut child, Duration::from_secs(20));
        let Some(status) = status else {
            let _ = child.kill();
            panic!("serve was still running 20s after SIGINT");
        };
        assert!(
            status.success(),
            "an operator-requested stop should exit 0, got {status:?}"
        );
    }
}
