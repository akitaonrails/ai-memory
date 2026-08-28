//! Windows PowerShell compatibility-hook transport regression tests.

#![cfg(windows)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate should live under crates/ai-memory-hooks")
        .to_path_buf()
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn receive_one_request(listener: TcpListener) -> (String, Vec<u8>) {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "PowerShell hook never connected");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("mock hook server accept failed: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let mut request = Vec::new();
    let (body_start, body_len) = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).expect("read hook request");
        assert!(read > 0, "hook request ended before its headers");
        request.extend_from_slice(&chunk[..read]);
        if let Some(end) = header_end(&request) {
            let headers = String::from_utf8_lossy(&request[..end]);
            let len = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .expect("PowerShell request should carry Content-Length");
            break (end + 4, len);
        }
    };
    while request.len() < body_start + body_len {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).expect("read hook request body");
        assert!(read > 0, "hook request body ended early");
        request.extend_from_slice(&chunk[..read]);
    }

    stream
        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .unwrap();
    let headers = String::from_utf8(request[..body_start - 4].to_vec()).unwrap();
    let body = request[body_start..body_start + body_len].to_vec();
    (headers, body)
}

#[test]
fn powershell_hook_posts_json_as_utf8_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let receiver = thread::spawn(move || receive_one_request(listener));

    let payload = serde_json::json!({
        "session_id": "utf8-regression",
        "prompt": "中文记忆测试：行动完成 — Validação: não, memória, correção, ação"
    })
    .to_string();
    let script = repo_root()
        .join("hooks")
        .join("lib")
        .join("ai-memory-hook.ps1");
    let script = script.to_string_lossy().replace('\'', "''");
    let program = format!(
        ". '{script}'; function Read-AiMemoryStdin {{ $env:AI_MEMORY_TEST_PAYLOAD }}; \
         Invoke-AiMemoryHook -Event 'user-prompt' -Agent 'codex'"
    );
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &program,
        ])
        .env("AI_MEMORY_HOOK_URL", server)
        .env("AI_MEMORY_TEST_PAYLOAD", &payload)
        .output()
        .expect("run PowerShell hook");
    assert!(
        output.status.success(),
        "PowerShell hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (headers, body) = receiver.join().unwrap();
    assert!(
        headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case("Content-Type: application/json; charset=utf-8")),
        "request did not declare UTF-8 JSON: {headers}"
    );
    assert_eq!(body, payload.as_bytes());
    let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        parsed["prompt"],
        "中文记忆测试：行动完成 — Validação: não, memória, correção, ação"
    );
}
