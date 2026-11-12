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

#[cfg(windows)]
mod windows {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;
    use std::os::windows::process::CommandExt as _;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::bin;
    use crate::e2e_support::hermetic;

    #[rstest::rstest]
    #[case("stop")]
    #[case("pre-compact")]
    #[case("session-end")]
    #[case("post-tool-use")]
    fn hook_output_closes_while_the_background_upload_is_blocked(#[case] event: &str) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).expect("create data dir");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let address = listener.local_addr().expect("mock address");
        listener.set_nonblocking(true).expect("nonblocking accept");
        std::fs::write(
            data.join("config.toml"),
            format!("server_url = 'http://{address}'\n"),
        )
        .expect("write config");

        let transcript = tmp.path().join("rollout.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",",
                "\"info\":{\"total_token_usage\":{\"input_tokens\":100,\"output_tokens\":10}}}}\n"
            ),
        )
        .expect("write usage fixture");

        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "worker never connected"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept upload: {error}"),
                }
            };
            // `accept()` inherits the listener's non-blocking mode on Windows,
            // and Winsock ignores `SO_RCVTIMEO` (set_read_timeout) while a socket
            // is non-blocking, so `read_line` would return `WouldBlock` the moment
            // the detached drainer has connected but not yet written its request.
            // Put the socket back into blocking mode so the read timeout governs.
            stream
                .set_nonblocking(false)
                .expect("blocking accepted socket");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("read timeout");
            let mut request = BufReader::new(stream);
            let mut line = String::new();
            request.read_line(&mut line).expect("request line");
            assert!(line.starts_with("POST /hook/batch "), "{line}");
            let mut content_length = 0;
            loop {
                line.clear();
                assert_ne!(
                    request.read_line(&mut line).expect("header"),
                    0,
                    "header EOF"
                );
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse::<usize>().expect("content length");
                }
            }
            let mut body = vec![0; content_length];
            request.read_exact(&mut body).expect("request body");
            started_tx.send(()).expect("notify upload started");
            // The response stays blocked until the test has observed EOF on
            // the hook's pipes. Cleanup releases it even when EOF times out.
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
            let _ = request.get_mut().write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });

        let mut hook = hermetic(bin())
            .args(["--data-dir"])
            .arg(&data)
            .args(["hook", "--agent", "codex", "--event", event])
            .env("AI_MEMORY_HOME", tmp.path())
            .env("CODEX_HOME", tmp.path().join("codex"))
            .env("RUST_LOG", "off")
            .current_dir(tmp.path())
            .creation_flags(0x0800_0000)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn hook");
        let payload = serde_json::json!({
            "session_id": uuid::Uuid::new_v4().to_string(),
            "cwd": tmp.path(),
            "rollout_path": transcript,
            "tool_name": "exec_command",
            "tool_input": {"cmd": "echo fixture"},
            "tool_response": "fixture"
        });
        hook.stdin
            .take()
            .expect("hook stdin")
            .write_all(payload.to_string().as_bytes())
            .expect("write payload");
        let (output_tx, output_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let _ = output_tx.send(hook.wait_with_output());
        });
        let started = started_rx.recv_timeout(Duration::from_secs(5));
        let output = output_rx.recv_timeout(Duration::from_secs(1));
        let _ = release_tx.send(());
        server.join().expect("mock server");
        waiter.join().expect("hook waiter");
        started.expect("worker reached mock server");
        let output = output
            .expect("hook output must close before the worker gets its HTTP response")
            .expect("wait for hook");
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "{}");
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    }
}
