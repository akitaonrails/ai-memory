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
    run_hook_full_agent(data_dir, "claude-code", event, payload, capture_assistant)
}

fn run_hook_full_agent(
    data_dir: &Path,
    agent: &str,
    event: &str,
    payload: &[u8],
    capture_assistant: bool,
) -> Output {
    run_hook_at(
        data_dir,
        agent,
        event,
        payload,
        capture_assistant,
        &ai_memory_test_support::dead_http_endpoint(),
    )
}

fn run_hook_at(
    data_dir: &Path,
    agent: &str,
    event: &str,
    payload: &[u8],
    capture_assistant: bool,
    server: &str,
) -> Output {
    let mut args = vec![
        "hook".to_string(),
        "--event".to_string(),
        event.to_string(),
        "--agent".to_string(),
        agent.to_string(),
        "--server-url".to_string(),
        server.to_owned(),
    ];
    if capture_assistant {
        args.push("--capture-assistant".to_string());
    }
    // A developer's own `~/.ai-memory.toml` applies to every cwd, so the
    // hook would annotate these payloads; the empty data dir stands in for
    // a home without one.
    let mut child = Command::new(bin())
        .env("AI_MEMORY_HOME", data_dir)
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
fn native_hook_recovers_a_response_backlog_and_does_not_advance_on_spool_failure() {
    for agent in ["claude-code", "codex"] {
        let tmp = tempfile::tempdir().unwrap();
        let transcript = tmp.path().join("transcript.jsonl");
        let text=(1..=300).map(|n| {
            let row=if agent=="claude-code" {
                serde_json::json!({"type":"assistant","message":{"id":format!("r-{n}"),"usage":{"input_tokens":100,"output_tokens":20}}})
            } else {
                serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100*n,"output_tokens":20*n}}}})
            };
            format!("{row}\n")
        }).collect::<String>();
        std::fs::write(&transcript, text).unwrap();
        let payload=serde_json::json!({"session_id":"native-backlog","cwd":tmp.path(),"transcript_path":transcript,"tool_name":"Bash","tool_input":{"command":"echo test"},"tool_response":"test"}).to_string();
        let endpoint = ai_memory_test_support::dead_http_endpoint();
        std::fs::write(tmp.path().join("hook-spool"), "unwritable spool fixture").unwrap();
        let failed = run_hook_at(
            tmp.path(),
            agent,
            "post-tool-use",
            payload.as_bytes(),
            false,
            &endpoint,
        );
        assert!(failed.status.success());
        std::fs::remove_file(tmp.path().join("hook-spool")).unwrap();
        let mut ids = std::collections::HashSet::new();
        for expected in [256, 300, 300] {
            let output = run_hook_at(
                tmp.path(),
                agent,
                "post-tool-use",
                payload.as_bytes(),
                false,
                &endpoint,
            );
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            for entry in spool_entries(tmp.path()) {
                let v: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
                let body: serde_json::Value =
                    serde_json::from_str(v["body"].as_str().unwrap()).unwrap();
                if let Some(calls) = body["_ai_memory_usage_calls"].as_array() {
                    assert!(calls.len() <= 256);
                    ids.extend(
                        calls
                            .iter()
                            .map(|c| c["call_id"].as_str().unwrap().to_owned()),
                    );
                }
            }
            assert_eq!(
                ids.len(),
                expected,
                "{agent} must recover the backlog across separate hook processes"
            );
        }
    }
}

#[test]
fn native_hook_reports_live_usage_for_every_supported_event_and_harness() {
    for agent in ["claude-code", "codex"] {
        for event in [
            "post-tool-use",
            "stop",
            "pre-compact",
            "session-end",
            "pre-tool-use",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let transcript = tmp.path().join("transcript.jsonl");
            let row = if agent == "claude-code" {
                serde_json::json!({"type":"assistant","message":{"id":"response-1","model":"fixture-model","usage":{"input_tokens":100,"output_tokens":20,"cache_read_input_tokens":80,"cache_creation_input_tokens":10}}})
            } else {
                serde_json::json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"output_tokens":20,"cached_input_tokens":800},"last_token_usage":{"input_tokens":1000,"output_tokens":20,"cached_input_tokens":800}}}})
            };
            std::fs::write(&transcript, format!("{row}\n{row}\n")).unwrap();
            let payload = serde_json::json!({"session_id":"live-native-hook","cwd":tmp.path(),"transcript_path":transcript,"tool_name":"Bash","tool_input":{"command":"echo fixture"},"tool_response":"fixture"});
            let output = run_hook_full_agent(
                tmp.path(),
                agent,
                event,
                payload.to_string().as_bytes(),
                false,
            );
            assert!(
                output.status.success(),
                "{agent}/{event}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let delivered: serde_json::Value =
                serde_json::from_str(&spooled_body(tmp.path())).unwrap();
            if event == "pre-tool-use" {
                assert!(delivered.get("_ai_memory_usage_calls").is_none());
                continue;
            }
            let calls = delivered["_ai_memory_usage_calls"]
                .as_array()
                .unwrap_or_else(|| panic!("{agent}/{event} did not report live usage"));
            assert_eq!(calls.len(), 1, "A replay is not a second response");
            assert_eq!(
                calls[0]["input_tokens"],
                if agent == "codex" { 200 } else { 100 }
            );
            assert_eq!(
                calls[0]["cache_read_tokens"],
                if agent == "codex" { 800 } else { 80 }
            );
            assert_eq!(calls[0]["output_tokens"], 20);
        }
    }
}

#[test]
fn claude_code_subagent_event_leaves_the_main_transcript_lines_for_the_next_event() {
    // The router drops a subagent event (`drop_subagent_captures`), so a
    // subagent event that read the main transcript would advance the cursor
    // past a response no later event reports.
    let tmp = tempfile::tempdir().unwrap();
    let transcript = tmp.path().join("transcript.jsonl");
    let row = serde_json::json!({"type":"assistant","message":{"id":"response-1","model":"fixture-model","usage":{"input_tokens":100,"output_tokens":20}}});
    // The last response stays pending until a later row closes it.
    let next = serde_json::json!({"type":"user","message":{"role":"user","content":"next"}});
    std::fs::write(&transcript, format!("{row}\n{next}\n")).unwrap();
    let endpoint = ai_memory_test_support::dead_http_endpoint();
    let send = |agent_id: Option<&str>| {
        let mut payload = serde_json::json!({"session_id":"subagent-usage","cwd":tmp.path(),"transcript_path":transcript,"tool_name":"Bash","tool_input":{"command":"echo fixture"},"tool_response":"fixture"});
        if let Some(id) = agent_id {
            payload["agent_id"] = id.into();
        }
        let output = run_hook_at(
            tmp.path(),
            "claude-code",
            "post-tool-use",
            payload.to_string().as_bytes(),
            false,
            &endpoint,
        );
        assert!(output.status.success());
    };
    send(Some("agent-abc123"));
    send(None);

    let bodies: Vec<serde_json::Value> = spool_entries(tmp.path())
        .iter()
        .map(|entry| {
            let v: serde_json::Value =
                serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap();
            serde_json::from_str(v["body"].as_str().unwrap()).unwrap()
        })
        .collect();
    assert_eq!(bodies.len(), 2);
    let reporting: Vec<_> = bodies
        .iter()
        .filter(|body| body.get("_ai_memory_usage_calls").is_some())
        .collect();
    assert_eq!(reporting.len(), 1, "the response is reported exactly once");
    assert!(
        reporting[0].get("agent_id").is_none(),
        "the subagent event must not carry the main conversation's response"
    );
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
fn oversized_core_bodies_are_capped_before_the_local_spool() {
    for (event, field, cap) in [
        (
            "user-prompt-submit",
            "prompt",
            ai_memory_hooks::USER_PROMPT_EXCERPT_MAX_BYTES,
        ),
        (
            "notification",
            "message",
            ai_memory_hooks::NOTIFICATION_EXCERPT_MAX_BYTES,
        ),
        (
            "post-compaction",
            "summary",
            ai_memory_hooks::POST_COMPACTION_EXCERPT_MAX_BYTES,
        ),
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let payload = serde_json::json!({
            "session_id": "bounded-spool",
            (field): format!("{}TAIL_SENTINEL", "x".repeat(cap + 8))
        })
        .to_string();
        let output = run_hook_event(tmp.path(), event, payload.as_bytes());
        assert!(
            output.status.success(),
            "{event}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let body = spooled_body(tmp.path());
        assert!(
            !body.contains("TAIL_SENTINEL"),
            "{event} spool was not capped"
        );
        let json: serde_json::Value =
            serde_json::from_str(&body).expect("spooled body remains JSON");
        let excerpt = json[field].as_str().expect("body field is text");
        assert!(excerpt.len() <= cap, "{event} exceeded {cap} bytes");
        assert!(excerpt.ends_with('…'));
    }
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

// --- Codex assistant-capture runtime (#196 / #743) -------------------------
//
// The INSTALLER baking `--capture-assistant` for Codex is unit-tested in
// `install_hooks.rs`; the RUNTIME (a Codex `Stop` payload carrying
// `last_assistant_message` flowing through the hook ingress, being captured and
// sanitized) was covered only for `--agent claude-code` above. Codex's Stop hook
// carries the assistant's final turn under the SAME top-level field spelling as
// Claude Code — `last_assistant_message` (verified on codex-cli 0.154.0, #743) —
// so the wire shape is identical and the ONLY runtime difference is `--agent
// codex`. These cases prove the Codex path end-to-end at the client spool, the
// same layer the Claude cases assert against (dead endpoint, zero-LLM, CI-safe).

#[test]
fn codex_opted_in_stop_captures_and_sanitizes_assistant_message() {
    // With --capture-assistant, a Codex Stop event spools the sanitized protocol
    // marker (NOT the raw field), carries capture_assistant=1 on a codex URL, and
    // a planted secret in the assistant message is redacted before it is spooled.
    let tmp = tempfile::tempdir().expect("tempdir");
    // AWS access-key shape (AKIA + 16 chars) → redacted by the built-in sanitizer,
    // mirroring `assistant_capture::client_transform_scrubs_before_truncating`.
    let secret = format!("AKIA{}", "A".repeat(16));
    // A unique, non-secret phrase proves the excerpt actually persisted, so the
    // secret-absence below reflects redaction rather than an empty capture.
    let survivor = "codex wrapped up the refactor";
    let payload = serde_json::json!({
        "session_id": "codex-capture",
        "cwd": "/tmp/project",
        "last_assistant_message": format!("{survivor} using key {secret}")
    })
    .to_string();

    let output = run_hook_full_agent(tmp.path(), "codex", "stop", payload.as_bytes(), true);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"{}\n");

    let entry = spooled_entry(tmp.path());
    let body = entry["body"].as_str().expect("spooled body");
    let url = entry["url"].as_str().expect("spooled url");

    // 1. Captured: the sanitized protocol marker is spliced in, the raw field is
    //    gone, and the server-gating flag rides the codex URL.
    assert!(
        !body.contains("last_assistant_message"),
        "raw assistant field survived: {body}"
    );
    assert!(
        body.contains("_ai_memory_assistant"),
        "protocol marker missing from codex capture: {body}"
    );
    assert!(
        body.contains(survivor),
        "non-secret excerpt did not persist (capture may be empty): {body}"
    );
    assert!(
        url.contains("capture_assistant=1"),
        "capture flag missing from codex url: {url}"
    );
    assert!(
        url.contains("agent=codex"),
        "codex agent missing from url: {url}"
    );

    // 2. Sanitized: the planted secret must not survive anywhere in the spool
    //    file bytes, and the sanitizer's redaction marker must be present.
    let entries = spool_entries(tmp.path());
    assert_eq!(entries.len(), 1);
    let raw_bytes = std::fs::read(entries[0].path()).expect("read spool entry");
    let raw_text = String::from_utf8_lossy(&raw_bytes);
    assert!(
        !raw_text.contains(&secret),
        "planted secret leaked into the codex spool file bytes"
    );
    assert!(
        body.contains("[REDACTED:aws_key]"),
        "secret was not redacted in the captured excerpt: {body}"
    );
}

#[test]
fn codex_capture_off_does_not_capture_assistant_message() {
    // Opt-in gating for Codex: WITHOUT --capture-assistant, the raw field is
    // stripped defensively, no protocol marker is spliced, and the URL carries no
    // capture flag — proving the flag actually gates Codex capture. Mirrors the
    // claude-code `stop_hook_strips_last_assistant_message_from_spool_and_stderr`.
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload = br#"{"session_id":"codex-off","cwd":"/tmp/project","last_assistant_message":"SENTINEL_ASSISTANT_MESSAGE"}"#;

    let output = run_hook_full_agent(tmp.path(), "codex", "stop", payload, false);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"{}\n");
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let entry = spooled_entry(tmp.path());
    let body = entry["body"].as_str().expect("spooled body");
    let url = entry["url"].as_str().expect("spooled url");
    assert!(
        !body.contains("SENTINEL_ASSISTANT_MESSAGE"),
        "assistant message captured with capture OFF: {body}"
    );
    assert!(
        !body.contains("last_assistant_message"),
        "raw assistant field key survived: {body}"
    );
    assert!(
        !body.contains("_ai_memory_assistant"),
        "protocol marker spliced with capture OFF: {body}"
    );
    assert!(
        !url.contains("capture_assistant=1"),
        "capture flag present with capture OFF: {url}"
    );
    // Unrelated fields survive so the Codex Stop event is still ingested.
    assert!(body.contains("codex-off"), "session_id was dropped: {body}");
}

// --- Lone UTF-16 surrogate escapes -----------------------------------------
//
// A harness that cuts tool output in the middle of an emoji can send half of a
// `\uD83D\uDE00` pair. `serde_json` rejects that escape, and so does the
// server's JSON extractor, so the hook replaces only the unpaired half with
// U+FFFD instead of dropping the whole event.

fn spooled_stdout(tmp: &Path, payload: &[u8]) -> String {
    let output = run_hook_event(tmp, "post-tool-use", payload);
    assert!(output.status.success());
    assert!(
        output.stderr.is_empty(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: serde_json::Value =
        serde_json::from_str(&spooled_body(tmp)).expect("spooled body is JSON");
    body["tool_response"]["stdout"]
        .as_str()
        .expect("tool_response.stdout")
        .to_owned()
}

#[test]
fn lone_leading_surrogate_escape_is_spooled_as_replacement_character() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload = br#"{"session_id":"lone-lead","cwd":"/tmp","tool_name":"Bash","tool_input":{"command":"echo"},"tool_response":{"stdout":"ok \ud83d\n... (truncated)"}}"#;
    assert_eq!(
        spooled_stdout(tmp.path(), payload),
        "ok \u{FFFD}\n... (truncated)"
    );
}

#[test]
fn lone_trailing_surrogate_escape_is_spooled_as_replacement_character() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload = br#"{"session_id":"lone-trail","cwd":"/tmp","tool_name":"Bash","tool_input":{"command":"echo"},"tool_response":{"stdout":"\ude00 tail"}}"#;
    assert_eq!(spooled_stdout(tmp.path(), payload), "\u{FFFD} tail");
}

#[test]
fn surrogate_repair_keeps_pairs_and_escaped_backslashes() {
    // A lead followed by a full pair: only the first lead is unpaired. The
    // `\\ud83d` is an escaped backslash followed by text, not an escape.
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload = br#"{"session_id":"mixed","cwd":"/tmp","tool_name":"Bash","tool_input":{"command":"echo"},"tool_response":{"stdout":"\ud83d\ud83d\ude00 \\ud83d \\\ude00"}}"#;
    assert_eq!(
        spooled_stdout(tmp.path(), payload),
        "\u{FFFD}\u{1F600} \\ud83d \\\u{FFFD}"
    );
}

#[test]
fn payload_with_a_full_surrogate_pair_keeps_its_exact_bytes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let payload = br#"{"session_id":"pair","cwd":"/tmp","tool_name":"Bash","tool_input":{"command":"echo"},"tool_response":{"stdout":"ok \ud83d\ude00"}}"#;
    let output = run_hook_event(tmp.path(), "post-tool-use", payload);
    assert!(output.status.success());
    assert_eq!(spooled_body(tmp.path()).as_bytes(), payload);
}

#[test]
fn malformed_payload_with_a_lone_surrogate_still_warns_without_spooling() {
    // Invalid JSON stays invalid after the repair; a backslash before a
    // multibyte character or at the end must not crash the hook.
    for payload in [
        r#"{"x":"\ud83d \é"#.as_bytes(),
        r#"{"x":"\ud83d \"#.as_bytes(),
    ] {
        let tmp = tempfile::tempdir().expect("tempdir");
        let output = run_hook(tmp.path(), payload);
        assert!(output.status.success());
        assert_eq!(output.stdout, b"{}\n");
        assert_eq!(
            String::from_utf8(output.stderr).expect("stderr utf8"),
            "ai-memory hook warning: could not parse event payload as JSON; nothing was captured\n"
        );
        assert!(!tmp.path().join("hook-spool").exists());
    }
}
