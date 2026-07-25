//! Native hook stdin parsing regressions, including PowerShell's UTF-8 BOM.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

fn run_hook(data_dir: &Path, payload: &[u8]) -> Output {
    run_hook_event(data_dir, "pre-tool-use", payload)
}

fn run_hook_event(data_dir: &Path, event: &str, payload: &[u8]) -> Output {
    run_hook_full(data_dir, event, payload, false)
}

fn run_hook_full(data_dir: &Path, event: &str, payload: &[u8], capture_assistant: bool) -> Output {
    let mut args = vec![
        "hook".to_string(),
        "--event".to_string(),
        event.to_string(),
        "--agent".to_string(),
        "claude-code".to_string(),
        "--server-url".to_string(),
        "http://127.0.0.1:1".to_string(),
    ];
    if capture_assistant {
        args.push("--capture-assistant".to_string());
    }
    let mut child = Command::new(bin())
        .args(["--data-dir"])
        .arg(data_dir)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn native hook");
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(payload)
        .expect("write hook payload");
    child.wait_with_output().expect("wait for native hook")
}

fn spool_entries(data_dir: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(data_dir.join("hook-spool"))
        .expect("hook spool")
        .collect::<Result<Vec<_>, _>>()
        .expect("spool entries")
        .into_iter()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .collect()
}

fn spooled_body(data_dir: &Path) -> String {
    let entries = spool_entries(data_dir);
    assert_eq!(entries.len(), 1);
    let entry: serde_json::Value =
        serde_json::from_slice(&std::fs::read(entries[0].path()).expect("read spool entry"))
            .expect("parse spool entry");
    entry["body"].as_str().expect("spooled body").to_owned()
}

fn spooled_entry(data_dir: &Path) -> serde_json::Value {
    // Use the filtered helper: boundary events spawn a detached drainer, so the
    // spool directory can also hold `.drain.lock` / `.json.tmp` while that child
    // is alive. Counting raw directory entries races with it (Windows loses most
    // often).
    let entries = spool_entries(data_dir);
    assert_eq!(entries.len(), 1);
    serde_json::from_slice(&std::fs::read(entries[0].path()).expect("read spool entry"))
        .expect("parse spool entry")
}

#[test]
fn native_hook_accepts_plain_and_bom_prefixed_json() {
    let payload = br#"{"session_id":"windows-test","cwd":"C:\\dev\\project","tool_name":"Read","tool_input":{"file_path":"README.md"}}"#;

    for with_bom in [false, true] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut stdin = Vec::new();
        if with_bom {
            stdin.extend_from_slice(&[0xef, 0xbb, 0xbf]);
        }
        stdin.extend_from_slice(payload);

        let output = run_hook(tmp.path(), &stdin);
        assert!(output.status.success());
        assert_eq!(output.stdout, b"{}\n");
        assert!(
            output.stderr.is_empty(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(spooled_body(tmp.path()).as_bytes(), payload);
    }
}

#[test]
fn malformed_native_hook_payload_warns_without_leaking_or_spooling() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = run_hook(
        tmp.path(),
        b"\xef\xbb\xbf{\"secret\":\"SENTINEL_PRIVATE_PAYLOAD\"",
    );

    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    assert_eq!(
        stderr,
        "ai-memory hook warning: could not parse event payload as JSON; nothing was captured\n"
    );
    assert!(!stderr.contains("SENTINEL_PRIVATE_PAYLOAD"));
    assert!(!tmp.path().join("hook-spool").exists());
}

#[test]
fn stop_hook_strips_last_assistant_message_from_spool_and_stderr() {
    // A well-formed Stop payload carrying Claude Code's `last_assistant_message`
    // must be spooled WITHOUT that raw field (#196). Optional capture remains
    // disabled, so the field must not reach the spool, wire, or stderr.
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload = br#"{"session_id":"stop-strip","cwd":"/tmp/project","last_assistant_message":"SENTINEL_ASSISTANT_MESSAGE"}"#;

    let output = run_hook_event(tmp.path(), "stop", payload);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let body = spooled_body(tmp.path());
    assert!(
        !body.contains("SENTINEL_ASSISTANT_MESSAGE"),
        "spooled body still carries the assistant message: {body}"
    );
    assert!(
        !body.contains("last_assistant_message"),
        "spooled body still carries the raw field key: {body}"
    );
    // Unrelated fields survive so the Stop event is still ingested.
    assert!(
        body.contains("stop-strip"),
        "session_id was dropped: {body}"
    );
}

#[test]
fn spool_files_never_leak_the_assistant_field_on_disk() {
    // Byte-level check across the whole spool file (not just the parsed body):
    // neither the value nor the raw key may survive anywhere in the entry.
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload =
        br#"{"session_id":"disk-scan","last_assistant_message":"SENTINEL_ASSISTANT_MESSAGE"}"#;

    let output = run_hook_event(tmp.path(), "stop", payload);
    assert!(output.status.success());

    let entries = spool_entries(tmp.path());
    assert_eq!(entries.len(), 1);
    let bytes = std::fs::read(entries[0].path()).expect("read spool entry");
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        !text.contains("SENTINEL_ASSISTANT_MESSAGE"),
        "assistant message leaked into the spool file bytes"
    );
    assert!(
        !text.contains("last_assistant_message"),
        "raw assistant field key leaked into the spool file bytes"
    );
}

#[test]
fn opted_in_stop_splices_protocol_and_capture_flag() {
    // With --capture-assistant, a Stop event spools the sanitized protocol
    // marker (NOT the raw field) and carries capture_assistant=1 on the URL so
    // the server can gate on it (#196).
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload = br#"{"session_id":"opt-in","last_assistant_message":"the fix is here"}"#;

    let output = run_hook_full(tmp.path(), "stop", payload, true);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");

    let entry = spooled_entry(tmp.path());
    let body = entry["body"].as_str().expect("spooled body");
    let url = entry["url"].as_str().expect("spooled url");
    assert!(
        !body.contains("last_assistant_message"),
        "raw field survived: {body}"
    );
    assert!(
        body.contains("_ai_memory_assistant"),
        "protocol marker missing: {body}"
    );
    assert!(
        body.contains("the fix is here"),
        "excerpt missing from protocol: {body}"
    );
    assert!(
        url.contains("capture_assistant=1"),
        "capture flag missing from url: {url}"
    );
}

#[test]
fn opted_in_non_stop_event_is_inert() {
    // The flag is a no-op on non-Stop events: no protocol, no capture flag, and
    // (absent any assistant field) the body is byte-exact.
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload = br#"{"session_id":"opt-in","prompt":"hello"}"#;

    let output = run_hook_full(tmp.path(), "user-prompt-submit", payload, true);
    assert!(output.status.success());

    let entry = spooled_entry(tmp.path());
    let url = entry["url"].as_str().expect("spooled url");
    assert!(
        !url.contains("capture_assistant=1"),
        "capture flag leaked onto a non-stop event: {url}"
    );
    assert_eq!(
        entry["body"].as_str().expect("spooled body").as_bytes(),
        payload,
        "unrelated event body must stay byte-exact"
    );
}

#[test]
fn every_spooled_event_carries_a_persistent_ingest_key() {
    // The idempotency key is minted ONCE at spool time and baked into the
    // entry's URL: every retry of that entry re-sends the same key, so the
    // server can recognize a replay whose previous delivery succeeded but
    // whose response was lost. Distinct events must carry distinct keys.
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload = br#"{"session_id":"idem","cwd":"/tmp/p","tool_name":"Read"}"#;

    let extract_key = |url: &str| -> String {
        url.split("ingest_key=")
            .nth(1)
            .and_then(|tail| tail.split('&').next())
            .unwrap_or_else(|| panic!("ingest_key missing from spooled url: {url}"))
            .to_string()
    };

    assert!(run_hook(tmp.path(), payload).status.success());
    assert!(run_hook(tmp.path(), payload).status.success());

    let entries = spool_entries(tmp.path());
    assert_eq!(entries.len(), 2);
    let keys: Vec<String> = entries
        .iter()
        .map(|e| {
            let entry: serde_json::Value =
                serde_json::from_slice(&std::fs::read(e.path()).expect("read spool entry"))
                    .expect("parse spool entry");
            extract_key(entry["url"].as_str().expect("spooled url"))
        })
        .collect();
    for key in &keys {
        assert!(
            (1..=64).contains(&key.len())
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "key must pass server-side validation: {key:?}"
        );
    }
    assert_ne!(keys[0], keys[1], "distinct events must mint distinct keys");
}
