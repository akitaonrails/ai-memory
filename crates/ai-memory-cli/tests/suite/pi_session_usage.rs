//! Generated Pi extension -> real isolated server -> session totals (no response ledger).

use std::path::Path;
use std::time::{Duration, Instant};

use ai_memory_core::SessionId;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};

use crate::e2e_support::{hermetic, start_serve};

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
const HARNESS: &str = include_str!("../fixtures/pi-session-usage.ts");

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Value {
    json!({"input": input, "output": output, "cacheRead": cache_read, "cacheWrite": cache_write})
}

fn assistant(id: &str, usage: Value) -> Value {
    json!({"type": "message", "id": id, "message": {"role": "assistant", "usage": usage}})
}

fn report(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Value {
    json!({
        "input_tokens": input, "output_tokens": output,
        "cache_read_tokens": cache_read, "cache_write_tokens": cache_write,
        "model": "pi-fixture-model",
    })
}

fn drive(root: &Path, extension: &Path, cwd: &Path, steps: Value) -> Value {
    let scenario = root.join("scenario.json");
    std::fs::write(&scenario, json!({"cwd": cwd, "steps": steps}).to_string()).unwrap();
    let out = hermetic("node")
        .args(["--experimental-strip-types", "--no-warnings"])
        .arg(root.join("harness.ts"))
        .arg(extension)
        .arg(scenario)
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("AI_MEMORY_HOME", root)
        .env("AI_MEMORY_DATA_DIR", root.join("client"))
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "Pi fixture failed:\n{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn stored(root: &Path, session: &str) -> Option<(u64, u64, u64, u64, bool)> {
    let conn = rusqlite::Connection::open_with_flags(
        root.join("server/db/memory.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    conn.busy_timeout(Duration::from_secs(5)).unwrap();
    conn.query_row(
        "SELECT u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens, \
         s.ended_at IS NOT NULL FROM session_usage u JOIN sessions s ON s.id = u.session_id \
         WHERE s.id = ?1",
        [SessionId::from_native(session).as_bytes().as_slice()],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )
    .optional()
    .unwrap()
}

async fn expect_stored(root: &Path, session: &str, want: (u64, u64, u64, u64, bool)) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let seen = stored(root, session);
        if seen == Some(want) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{session}: want {want:?}, got {seen:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pi_session_totals_survive_replay_reload_compaction_and_session_switches() {
    if !hermetic("node")
        .args(["--experimental-strip-types", "--version"])
        .output()
        .is_ok_and(|out| out.status.success())
    {
        eprintln!("skipping: node lacks --experimental-strip-types");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let project = root.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(".ai-memory.toml"),
        "workspace = \"pi-usage\"\nproject = \"home\"\n",
    )
    .unwrap();
    std::fs::write(root.join("harness.ts"), HARNESS).unwrap();
    let client = reqwest::Client::new();
    let (_server, base) = start_serve(&client, &root.join("serve.log"), |port| {
        let mut cmd = hermetic(BIN);
        cmd.args([
            "serve",
            "--transport",
            "http",
            "--bind",
            &format!("127.0.0.1:{port}"),
            "--workspace",
            "pi-usage",
            "--project",
            "home",
            "--no-watcher",
        ])
        .env("AI_MEMORY_DATA_DIR", root.join("server"))
        .env("AI_MEMORY_HOME", root)
        .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
        .current_dir(&project);
        cmd
    })
    .await;
    let extension = root.join("extension.ts");
    let installed = hermetic(BIN)
        .args(["install-hooks", "--agent", "pi", "--apply", "--config-file"])
        .arg(&extension)
        .args(["--server-url", &base])
        .env("AI_MEMORY_HOME", root)
        .env("AI_MEMORY_DATA_DIR", root.join("client"))
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    // Test-only export lets the fixture wait for delivery without changing production hooks.
    let source = std::fs::read_to_string(&extension).unwrap();
    std::fs::write(
        &extension,
        format!("{source}\nexport {{ drainHookQueueForDispose }};\n"),
    )
    .unwrap();

    let mut a = assistant("a", usage(100, 20, 80, 10));
    a["message"]["content"] = json!([{"type": "text", "text": "PRIVATE_HISTORY_MUST_NOT_POST"}]);
    a["message"]["usage"]["reasoning"] = json!(3);
    a["message"]["usage"]["cacheWrite1h"] = json!(8);
    let hooks = drive(
        root,
        &extension,
        &project,
        json!([
            {"event": "session_start", "session": "pi-main", "entries": []},
            // Streaming events are not a second source of usage; only persisted final entries count.
            {"event": "message_end", "session": "pi-main", "entries": [], "payload": {"message": a["message"]}},
            {"event": "agent_end", "hook": "stop", "session": "pi-main", "entries": [a], "expected": report(100, 20, 80, 10)},
            {"event": "agent_end", "hook": "stop", "session": "pi-main", "entries": [a], "expected": report(100, 20, 80, 10)}
        ]),
    );
    assert!(!hooks.to_string().contains("PRIVATE_HISTORY_MUST_NOT_POST"));
    expect_stored(root, "pi-main", (100, 20, 80, 10, false)).await;

    drive(
        root,
        &extension,
        &project,
        json!([
            {"event": "session_shutdown", "hook": "session-end", "session": "pi-main", "entries": [a], "payload": {"reason": "reload"}, "expected": report(100, 20, 80, 10)}
        ]),
    );
    // A fresh Node process after shutdown models /reload or restart/resume.
    let history = json!([
        a,
        assistant("b", usage(30, 4, 200, 0)),
        {"type": "compaction", "id": "c", "usage": usage(7, 2, 1, 3)},
        {"type": "branch_summary", "id": "d", "usage": usage(5, 1, 2, 0)},
        {"type": "usage", "id": "e", "kind": "future_kind", "usage": usage(0, 0, 50, 0)},
        {"type": "message", "id": "f", "message": {"role": "toolResult", "usage": usage(9, 3, 0, 0)}},
        {"type": "message", "id": "g", "message": {"role": "assistant"}},
        {"type": "custom", "id": "h", "usage": usage(999, 999, 999, 999)},
        {"type": "message", "id": "i", "message": {"role": "user", "usage": usage(999, 999, 999, 999)}},
        assistant("negative", json!({"input": -1, "output": 999, "cacheRead": 0, "cacheWrite": 0})),
        assistant("fractional", json!({"input": 1.5, "output": 999, "cacheRead": 0, "cacheWrite": 0})),
        assistant("string", json!({"input": "1", "output": 999, "cacheRead": 0, "cacheWrite": 0})),
        assistant("zero", usage(0, 0, 0, 0))
    ]);
    drive(
        root,
        &extension,
        &project,
        json!([
            {"event": "session_start", "session": "pi-main", "entries": history, "payload": {"reason": "reload"}},
            {"event": "agent_end", "hook": "stop", "session": "pi-main", "entries": history, "expected": report(151, 30, 333, 13)},
            {"event": "agent_end", "hook": "stop", "session": "pi-other", "entries": [assistant("other", usage(2, 1, 0, 0))], "expected": report(2, 1, 0, 0)},
            // Pi forks copy a path into another session; its totals follow that copied history, not the parent's other branches.
            {"event": "agent_end", "hook": "stop", "session": "pi-fork", "entries": [a, assistant("fork", usage(1, 2, 3, 4))], "expected": report(101, 22, 83, 14)},
            {"event": "session_shutdown", "hook": "session-end", "session": "pi-other", "entries": [assistant("other", usage(2, 1, 0, 0))], "expected": report(2, 1, 0, 0)},
            {"event": "agent_end", "hook": "stop", "session": "pi-unknown", "entries": [assistant("missing", Value::Null)], "expected": null},
            {"event": "session_shutdown", "hook": "session-end", "session": "pi-main", "entries": history, "expected": report(151, 30, 333, 13)}
        ]),
    );
    expect_stored(root, "pi-main", (151, 30, 333, 13, true)).await;
    expect_stored(root, "pi-other", (2, 1, 0, 0, true)).await;
    expect_stored(root, "pi-fork", (101, 22, 83, 14, false)).await;
    assert_eq!(
        stored(root, "pi-unknown"),
        None,
        "missing usage must stay unknown"
    );

    // A delayed older snapshot cannot lower totals, even after the session ended.
    drive(
        root,
        &extension,
        &project,
        json!([
            {"event": "agent_end", "session": "pi-main", "entries": [a]},
            {"event": "agent_end", "session": "pi-main", "entries": []},
            {"event": "agent_end", "session": "pi-control", "entries": [assistant("control", usage(4, 2, 0, 0))]}
        ]),
    );
    expect_stored(root, "pi-control", (4, 2, 0, 0, false)).await;
    assert_eq!(stored(root, "pi-main"), Some((151, 30, 333, 13, true)));
    let conn = rusqlite::Connection::open(root.join("server/db/memory.sqlite")).unwrap();
    let calls: i64 = conn
        .query_row("SELECT COUNT(*) FROM token_calls", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        calls, 0,
        "session totals must not fabricate response ledger rows"
    );
}
