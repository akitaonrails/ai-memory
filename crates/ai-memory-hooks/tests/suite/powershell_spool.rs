//! Windows PowerShell offline-spool regression tests: the `.ps1` bundle
//! must survive a server outage the way the shell bundle and the native
//! hooks do (#580 parity) — spool an undeliverable event in the shared
//! `hook-spool` on-disk contract, and drain the backlog (retiring on 2xx
//! or terminal 4xx, keeping it on connection failure).

#![cfg(windows)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use ai_memory_test_support::powershell_exe;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate should live under crates/ai-memory-hooks")
        .to_path_buf()
}

fn hook_lib() -> String {
    repo_root()
        .join("hooks")
        .join("lib")
        .join("ai-memory-hook.ps1")
        .to_string_lossy()
        .replace('\'', "''")
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Read one full HTTP request and answer it with `response`. A cold
/// PowerShell start on a loaded windows-latest runner can take well over
/// ten seconds before opening a connection, so the accept window only
/// bounds failure detection (same posture as `powershell_utf8.rs`).
fn serve_one(listener: &TcpListener, response: &[u8]) -> (String, Vec<u8>) {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut stream, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "PowerShell drain never connected"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("mock drain server accept failed: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let mut request = Vec::new();
    let (body_start, body_len) = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).expect("read drain request");
        assert!(read > 0, "drain request ended before its headers");
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
                .expect("drain request should carry Content-Length");
            break (end + 4, len);
        }
    };
    while request.len() < body_start + body_len {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).expect("read drain request body");
        assert!(read > 0, "drain request body ended early");
        request.extend_from_slice(&chunk[..read]);
    }

    stream.write_all(response).unwrap();
    let headers = String::from_utf8(request[..body_start - 4].to_vec()).unwrap();
    let body = request[body_start..body_start + body_len].to_vec();
    (headers, body)
}

/// Run a PowerShell program with HOME/USERPROFILE and the data dir pinned
/// inside `home`, returning the process output.
fn run_powershell(program: &str, home: &Path, envs: &[(&str, &str)]) -> std::process::Output {
    let mut command = Command::new(powershell_exe());
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            program,
        ])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("AI_MEMORY_AUTH_TOKEN")
        .env_remove("AI_MEMORY_HOOK_URL")
        .env_remove("AI_MEMORY_SESSION_ID")
        .env_remove("AI_MEMORY_CAPTURE_OWNER");
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().expect("run PowerShell")
}

fn spool_files(data_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(data_dir.join("hook-spool"))
        .expect("spool dir should exist")
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    files
}

#[test]
fn powershell_hook_spools_when_server_unreachable() {
    // A reserved port that never listens: connection refused, and no
    // parallel test can take it (#2030).
    let refusing = ai_memory_test_support::refusing_port();
    let server = refusing.url();

    let home = tempfile::tempdir().unwrap();
    let data_dir = home.path().join("data");
    let payload = r#"{"session_id":"ps-spool-regression","prompt":"outage capture"}"#;
    let program = format!(
        ". '{lib}'; function Read-AiMemoryStdin {{ $env:AI_MEMORY_TEST_PAYLOAD }}; \
         Invoke-AiMemoryHook -Event 'user-prompt' -Agent 'codex'",
        lib = hook_lib()
    );
    let output = run_powershell(
        &program,
        home.path(),
        &[
            ("AI_MEMORY_HOOK_URL", server.as_str()),
            ("AI_MEMORY_TEST_PAYLOAD", payload),
            ("AI_MEMORY_DATA_DIR", data_dir.to_str().unwrap()),
        ],
    );
    assert!(
        output.status.success(),
        "PowerShell hook failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "hook must keep stdout empty on this path: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let files = spool_files(&data_dir);
    assert_eq!(files.len(), 1, "one spool entry after a refused POST");
    let name = files[0].file_name().unwrap().to_string_lossy().to_string();
    let stem = name.strip_suffix(".json").unwrap_or_default();
    let segments = stem.split('-').collect::<Vec<_>>();
    assert!(
        segments.len() == 3
            && segments[0].len() == 13
            && segments[0].bytes().all(|byte| byte.is_ascii_digit())
            && !segments[1].is_empty()
            && segments[1].bytes().all(|byte| byte.is_ascii_digit())
            && segments[2].len() == 16
            && segments[2].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "entry name follows <ms:013>-<pid>-<seq:x016>: {name}"
    );

    let raw = std::fs::read(&files[0]).unwrap();
    assert!(
        raw.first() != Some(&0xEF),
        "entry must be UTF-8 without a BOM for the native drainer"
    );
    let entry: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    let url = entry["url"].as_str().unwrap();
    assert!(
        url.contains("/hook?event=user-prompt&agent=codex") && url.contains("&ingest_key=ps"),
        "spooled url carries the event/agent query and an idempotency key: {url}"
    );
    assert!(
        url.starts_with(&server),
        "spooled url points at the attempted server: {url} vs {server}"
    );
    assert_eq!(entry["body"].as_str(), Some(payload));
    assert_eq!(entry["auth_mode"].as_str(), Some("none"));
    assert_eq!(entry["attempts"].as_u64(), Some(0));
    assert!(
        entry.get("token").is_none() || entry["token"].is_null(),
        "no token stored when the hook had none"
    );
}

#[test]
fn powershell_drain_delivers_and_retires_entries() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let receiver = thread::spawn(move || {
        // 202 retires the first entry; a terminal 400 retires the second
        // (permanent rejection, not retried).
        let (headers1, body1) = serve_one(
            &listener,
            b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let (headers2, body2) = serve_one(
            &listener,
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        (headers1, body1, headers2, body2)
    });

    let home = tempfile::tempdir().unwrap();
    let data_dir = home.path().join("data");
    let spool = data_dir.join("hook-spool");
    std::fs::create_dir_all(&spool).unwrap();
    let body_one = r#"{"e":"backlog-one"}"#;
    let body_two = r#"{"e":"backlog-two"}"#;
    // Entry one carries a static bearer (the shape the shell bundle and the
    // native hooks write); entry two is anonymous.
    std::fs::write(
        spool.join("0000000000005-1-0000000000000001.json"),
        serde_json::json!({
            "url": format!("{server}/hook?event=stop&agent=codex&ingest_key=psaaaaaaaaaaaaaaaa"),
            "body": body_one,
            "created_ms": 5_u64,
            "auth_mode": "static",
            "token": "drain-bearer",
            "attempts": 0,
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        spool.join("0000000000006-1-0000000000000002.json"),
        serde_json::json!({
            "url": format!("{server}/hook?event=stop&agent=codex&ingest_key=psbbbbbbbbbbbbbbbb"),
            "body": body_two,
            "created_ms": 6_u64,
            "auth_mode": "none",
            "attempts": 0,
        })
        .to_string(),
    )
    .unwrap();

    let program = format!(
        ". '{lib}'; Invoke-AiMemoryDrainSpool -Max 8",
        lib = hook_lib()
    );
    let output = run_powershell(
        &program,
        home.path(),
        &[("AI_MEMORY_DATA_DIR", data_dir.to_str().unwrap())],
    );
    assert!(
        output.status.success(),
        "PowerShell drain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (headers1, body1, headers2, body2) = receiver.join().unwrap();
    assert!(
        headers1
            .lines()
            .any(|line| line.eq_ignore_ascii_case("Authorization: Bearer drain-bearer")),
        "first entry's static token must reach the server: {headers1}"
    );
    assert_eq!(body1, body_one.as_bytes());
    assert!(
        !headers2
            .lines()
            .any(|line| line.to_ascii_lowercase().starts_with("authorization:")),
        "anonymous entry must not send a bearer: {headers2}"
    );
    assert_eq!(body2, body_two.as_bytes());
    assert!(
        spool_files(&data_dir).is_empty(),
        "2xx and terminal 4xx both retire their entries"
    );
}

#[test]
fn powershell_drain_keeps_backlog_on_429_saturation() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let server = format!("http://{}", listener.local_addr().unwrap());
    let receiver = thread::spawn(move || {
        // 429 indicates server queue saturation (backpressure):
        // the drain must stop and keep the entry queued rather than retiring it.
        serve_one(
            &listener,
            b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
    });

    let home = tempfile::tempdir().unwrap();
    let data_dir = home.path().join("data");
    let spool = data_dir.join("hook-spool");
    std::fs::create_dir_all(&spool).unwrap();
    let body = r#"{"e":"saturated"}"#;
    std::fs::write(
        spool.join("0000000000007-1-0000000000000001.json"),
        serde_json::json!({
            "url": format!("{server}/hook?event=stop&agent=codex&ingest_key=pscccccccccccccccc"),
            "body": body,
            "created_ms": 7_u64,
            "auth_mode": "none",
            "attempts": 0,
        })
        .to_string(),
    )
    .unwrap();

    let program = format!(
        ". '{lib}'; Invoke-AiMemoryDrainSpool -Max 8",
        lib = hook_lib()
    );
    let output = run_powershell(
        &program,
        home.path(),
        &[("AI_MEMORY_DATA_DIR", data_dir.to_str().unwrap())],
    );
    assert!(
        output.status.success(),
        "PowerShell drain failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (_headers, received_body) = receiver.join().unwrap();
    assert_eq!(received_body, body.as_bytes());
    assert_eq!(
        spool_files(&data_dir).len(),
        1,
        "a 429 response must keep the entry queued"
    );
}
