//! End-to-end coverage for degraded offline launches: `ai-memory run` against
//! a closed port (a homelab down for maintenance) must WARN and still launch
//! the harness without the server, `--require-server` must fail closed with
//! the old augmented error, and offline auto-wire must install hooks but no
//! dead-server MCP registration. Unix-only: the fake harness is a shebang
//! script.

#![cfg(unix)]

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
type RecoveryServerState = (
    Arc<Mutex<Vec<Vec<Value>>>>,
    Arc<Mutex<Vec<(String, Value)>>>,
);

/// A loopback address nothing listens on: `free_port` binds and releases.
fn closed_port_url() -> String {
    format!("http://127.0.0.1:{}", crate::e2e_support::free_port())
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn host_tool(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} on PATH"))
}

struct Fixture {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    claude_ran: PathBuf,
    claude_env: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        let home = root.join("home");
        let bin = root.join("bin");
        for dir in [&repo, &home, &bin] {
            fs::create_dir_all(dir).unwrap();
        }
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git init");
        std::os::unix::fs::symlink(host_tool("git"), bin.join("git")).unwrap();

        let claude_ran = root.join("claude-ran.txt");
        let claude_env = root.join("claude-env.txt");
        // The fake harness records its argv and environment and writes the
        // (empty-session) transcript the launcher waits for, so the run
        // finishes instead of waiting out the transcript-flush poll. `set`
        // (a shell builtin) dumps the environment without needing anything
        // else on the replaced PATH.
        let transcripts = home
            .join(".claude/projects")
            .join(repo.to_string_lossy().replace('/', "-"));
        fs::create_dir_all(&transcripts).unwrap();
        write_script(
            &bin.join("claude"),
            &format!(
                "printf '%s\\n' \"$@\" > '{ran}'\nset > '{env_}'\n\
                 while [ $# -gt 0 ]; do\n\
                 if [ \"$1\" = --session-id ]; then\n\
                 printf '{{\"sessionId\":\"%s\",\"cwd\":\"%s\"}}\\n{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"offline exact prompt\"}}]}}}}\\n{{\"type\":\"assistant\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"offline exact reply\"}}]}}}}\\n' \"$2\" '{repo}' > '{dir}'/\"$2\".jsonl\n\
                 fi\nshift\ndone\nexit 0\n",
                ran = claude_ran.display(),
                env_ = claude_env.display(),
                repo = repo.display(),
                dir = transcripts.display(),
            ),
        );
        Self {
            _temp: temp,
            repo,
            home,
            bin,
            claude_ran,
            claude_env,
        }
    }
}

fn command(fixture: &Fixture, server: &str, args: &[&str]) -> tokio::process::Command {
    let mut command: tokio::process::Command = crate::e2e_support::hermetic(BIN).into();
    for name in ["SSH_AUTH_SOCK", "XDG_CONFIG_HOME", "GH_CONFIG_DIR", "PS1"] {
        command.env_remove(name);
    }
    command
        .args(args)
        .current_dir(&fixture.repo)
        .env("PATH", &fixture.bin)
        .env("HOME", &fixture.home)
        .env("AI_MEMORY_HOME", &fixture.home)
        .env("CLAUDE_CONFIG_DIR", fixture.home.join(".claude"))
        .env("AI_MEMORY_DATA_DIR", fixture.home.join("data"))
        .env("AI_MEMORY_SERVER_URL", server)
        .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

async fn run(fixture: &Fixture, server: &str, args: &[&str]) -> Output {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        command(fixture, server, args).output(),
    )
    .await
    .expect("ai-memory run finished")
    .expect("spawn ai-memory")
}

async fn recover(fixture: &Fixture, server: &str) -> Output {
    run(fixture, server, &["recover", "--json"]).await
}

fn journal_entries(fixture: &Fixture) -> Vec<Value> {
    let journal_path = fixture.home.join("data/recovery-journal.json");
    let journal: Value = serde_json::from_slice(&fs::read(journal_path).expect("recovery journal"))
        .expect("recovery journal JSON");
    journal["entries"]
        .as_array()
        .expect("journal entries")
        .clone()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Re-stamp the newest spool entry as a hook that fired inside the journal
/// interval `[0].interval_started_ms ..= interval_ended_ms`. Real harness
/// hooks spool while the child runs; the fake harness has none, so the test
/// enqueues after the fact and back-dates both the body and the file name
/// (whose leading milliseconds the tombstone cleaner trusts).
fn backdate_latest_spool_entry(fixture: &Fixture, ms: u64) {
    let spool = fixture.home.join("data/hook-spool");
    let mut newest: Option<(std::path::PathBuf, String)> = None;
    for entry in fs::read_dir(&spool).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();
        if newest.as_ref().is_none_or(|(_, other)| &name > other) {
            newest = Some((path, name));
        }
    }
    let Some((path, name)) = newest else {
        panic!("no spool entry to back-date");
    };
    let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    value["created_ms"] = json!(ms);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    let suffix = name.split_once('-').expect("spool name embeds its stamp").1;
    fs::rename(&path, spool.join(format!("{:013}-{suffix}", ms))).unwrap();
}

fn enqueue_hook(fixture: &Fixture, server: &str, session_id: &str, marker: &str) {
    let mut child = crate::e2e_support::hermetic(BIN)
        .args([
            "--data-dir",
            fixture.home.join("data").to_str().unwrap(),
            "hook",
            "--event",
            "user-prompt-submit",
            "--agent",
            "claude-code",
            "--server-url",
            server,
        ])
        .current_dir(&fixture.repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({
                "session_id": session_id,
                "cwd": fixture.repo,
                "prompt": marker,
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{}", stderr_text(&output));
}

async fn fake_recovery_server_on(
    listener: tokio::net::TcpListener,
    batches: Arc<Mutex<Vec<Vec<Value>>>>,
    finishes: Arc<Mutex<Vec<(String, Value)>>>,
) -> tokio::task::JoinHandle<()> {
    async fn health() -> StatusCode {
        StatusCode::OK
    }
    async fn batch(
        State((batches, _)): State<RecoveryServerState>,
        Json(items): Json<Vec<Value>>,
    ) -> Json<Value> {
        let accepted = items.len();
        let results = (0..accepted)
            .map(|index| json!({"index": index, "outcome": "stored"}))
            .collect::<Vec<_>>();
        batches.lock().unwrap().push(items);
        Json(json!({ "accepted": accepted, "results": results }))
    }
    async fn finish(
        State((_, finishes)): State<RecoveryServerState>,
        axum::extract::Path(run_id): axum::extract::Path<String>,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        finishes.lock().unwrap().push((run_id, body));
        Json(json!({ "imported_events": 1, "latest_sequence": 1 }))
    }

    let state = (batches, finishes);
    let app = Router::new()
        .route("/healthz", get(health))
        .route("/hook/batch", post(batch))
        .route("/workstream/runs/{run_id}/finish", post(finish))
        .route("/workstream/runs/{run_id}/recover/finish", post(finish))
        .with_state(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() })
}

/// A closed port downgrades the launch: one loud warning, the harness runs,
/// no run/workstream ids are attributed to the offline child, and the
/// not-recorded line accounts for the (empty) local spool.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_run_launches_the_harness_without_the_server() {
    let fixture = Fixture::new();
    let server = closed_port_url();
    let output = run(&fixture, &server, &["run", "--no-autowire", "claude"]).await;
    assert!(output.status.success(), "{}", stderr_text(&output));

    let output_stderr = stderr_text(&output);
    assert!(
        output_stderr.contains("WARNING") && output_stderr.contains("unreachable"),
        "the degraded warning is loud:\n{output_stderr}"
    );
    assert!(
        output_stderr.contains(&server),
        "the warning names the server URL:\n{output_stderr}"
    );
    assert!(
        output_stderr.contains("not recorded on the server"),
        "the spool account is printed:\n{output_stderr}"
    );
    assert!(
        output_stderr.contains("no hook events remain spooled locally"),
        "the empty spool is reported as empty:\n{output_stderr}"
    );

    assert!(
        fixture.claude_ran.exists(),
        "the harness ran:\n{output_stderr}"
    );
    let entries = journal_entries(&fixture);
    assert_eq!(
        entries.len(),
        1,
        "one offline run is journaled: {entries:?}"
    );
    assert_eq!(entries[0]["kind"], "degraded-run");
    assert_eq!(entries[0]["workspace"], "default");
    assert!(entries[0]["native_session_id"].as_str().is_some());
    let journal_text = serde_json::to_string(&entries).unwrap();
    assert!(!journal_text.contains("offline exact prompt"));
    let pending = recover(&fixture, &server).await;
    assert!(pending.status.success(), "{}", stderr_text(&pending));
    let report: Value = serde_json::from_slice(&pending.stdout).expect("recover JSON");
    assert_eq!(report["reachable"], false);
    assert_eq!(report["journal_remaining"], 1);
    assert_eq!(journal_entries(&fixture).len(), 1);

    let child_env = fs::read_to_string(&fixture.claude_env).unwrap();
    assert!(
        !child_env
            .lines()
            .any(|line| line.starts_with("AI_MEMORY_RUN_ID=")
                || line.starts_with("AI_MEMORY_WORKSTREAM_ID=")),
        "no server-run attribution may reach the offline child:\n{child_env}"
    );
    assert!(
        child_env
            .lines()
            .any(|line| line.starts_with("AI_MEMORY_HOOK_URL=")),
        "the hook URL still points at the configured server:\n{child_env}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_exporter_is_journaled_by_a_real_degraded_run_as_spool_only() {
    let fixture = Fixture::new();
    let conversation = "12345678-1234-4234-9234-123456789abc";
    let store = fixture.home.join(".gemini/antigravity-cli/conversations");
    fs::create_dir_all(&store).unwrap();
    let db = store.join(format!("{conversation}.db"));
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch("CREATE TABLE trajectory (trajectory_metadata_blob BLOB NOT NULL);")
        .unwrap();
    let uri = format!("file://{}", fixture.repo.display());
    let mut nested = Vec::new();
    nested.push(10);
    nested.push(u8::try_from(uri.len()).unwrap());
    nested.extend_from_slice(uri.as_bytes());
    let mut metadata = vec![10, u8::try_from(nested.len()).unwrap()];
    metadata.extend_from_slice(&nested);
    connection
        .execute(
            "INSERT INTO trajectory (trajectory_metadata_blob) VALUES (?1)",
            rusqlite::params![metadata],
        )
        .unwrap();
    drop(connection);
    write_script(&fixture.bin.join("agy"), "exit 0\n");

    let server = closed_port_url();
    let output = run(
        &fixture,
        &server,
        &[
            "run",
            "--no-autowire",
            "antigravity",
            "--conversation",
            conversation,
        ],
    )
    .await;
    assert!(output.status.success(), "{}", stderr_text(&output));
    let entries = journal_entries(&fixture);
    assert_eq!(entries.len(), 1, "real degraded run must be journaled");
    assert_eq!(entries[0]["harness"], "antigravity");
    assert_eq!(entries[0]["native_session_id"], conversation);
    assert_eq!(entries[0]["spool_only"], true);
    assert!(entries[0].get("event_digests").is_none());
    assert!(entries[0].get("final_cursor").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_refuses_same_event_ids_with_mutated_content() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_url = format!("http://{}", listener.local_addr().unwrap());
    let fixture = Fixture::new();
    let output = run(&fixture, &server_url, &["run", "--no-autowire", "claude"]).await;
    assert!(output.status.success(), "{}", stderr_text(&output));
    let entries = journal_entries(&fixture);
    let session_id = entries[0]["native_session_id"].as_str().unwrap();
    let transcript = fixture
        .home
        .join(".claude/projects")
        .join(fixture.repo.to_string_lossy().replace('/', "-"))
        .join(format!("{session_id}.jsonl"));
    let original = fs::read_to_string(&transcript).unwrap();
    fs::write(
        &transcript,
        original.replace("offline exact reply", "mutated exact reply"),
    )
    .unwrap();

    let batches = Arc::new(Mutex::new(Vec::new()));
    let finishes = Arc::new(Mutex::new(Vec::new()));
    let server = fake_recovery_server_on(listener, batches.clone(), finishes).await;
    let recovered = recover(&fixture, &server_url).await;
    assert!(
        !recovered.status.success(),
        "semantic mutation must refuse replay"
    );
    let report: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(report["journal_remaining"], 1);
    assert!(batches.lock().unwrap().is_empty());
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recover_drains_spool_then_replays_exact_degraded_transcript_idempotently() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server_url = format!("http://{}", listener.local_addr().unwrap());
    let fixture = Fixture::new();
    let output = run(&fixture, &server_url, &["run", "--no-autowire", "claude"]).await;
    assert!(output.status.success(), "{}", stderr_text(&output));
    let entries = journal_entries(&fixture);
    let session_id = entries[0]["native_session_id"]
        .as_str()
        .unwrap()
        .to_string();
    enqueue_hook(&fixture, &server_url, &session_id, "offline exact prompt");
    backdate_latest_spool_entry(
        &fixture,
        entries[0]["interval_ended_ms"]
            .as_u64()
            .expect("interval end"),
    );
    let transcript = fixture
        .home
        .join(".claude/projects")
        .join(fixture.repo.to_string_lossy().replace('/', "-"))
        .join(format!("{session_id}.jsonl"));
    let mut transcript_file = fs::OpenOptions::new()
        .append(true)
        .open(transcript)
        .unwrap();
    writeln!(
        transcript_file,
        "{}",
        json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"later resumed work"}]}})
    )
    .unwrap();

    let batches = Arc::new(Mutex::new(Vec::new()));
    let finishes = Arc::new(Mutex::new(Vec::new()));
    let server = fake_recovery_server_on(listener, batches.clone(), finishes.clone()).await;
    let recovered = recover(&fixture, &server_url).await;
    assert!(recovered.status.success(), "{}", stderr_text(&recovered));
    let report: Value = serde_json::from_slice(&recovered.stdout).expect("recover JSON");
    assert_eq!(report["spool"]["sent"], 0);
    assert_eq!(report["journal_remaining"], 0);
    assert!(journal_entries(&fixture).is_empty());

    let requests = batches.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        1,
        "the correlated spool entry must be quarantined and replaced by the transcript"
    );
    let replay = &requests[0];
    assert_eq!(replay.len(), 4, "start, exact user/reply, end: {replay:?}");
    assert!(
        replay[0]["url"]
            .as_str()
            .unwrap()
            .contains("event=session-start")
    );
    assert_eq!(replay[1]["body"]["prompt"], "offline exact prompt");
    assert_eq!(replay[2]["body"]["message"], "offline exact reply");
    assert!(
        replay.iter().all(|item| item["body"]
            .to_string()
            .find("later resumed work")
            .is_none()),
        "post-run resumed work must stay outside the captured upper bound: {replay:?}"
    );
    assert!(
        replay[3]["url"]
            .as_str()
            .unwrap()
            .contains("event=session-end")
    );
    assert!(replay.iter().all(|item| {
        item["url"]
            .as_str()
            .unwrap()
            .contains(&format!("session_id={session_id}"))
    }));
    assert!(finishes.lock().unwrap().is_empty());
    let recovery_root = fixture.home.join("data/hook-spool/.recovery");
    let recovery_dirs = fs::read_dir(&recovery_root)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(recovery_dirs.len(), 1, "a bounded tombstone remains");
    assert!(recovery_dirs[0].join(".completed").is_file());
    assert_eq!(
        fs::read_dir(&recovery_dirs[0])
            .unwrap()
            .filter(|entry| entry
                .as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|ext| ext == "json"))
            .count(),
        0,
        "confirmed replay deletes quarantined overlap but retains its tombstone"
    );

    let before = requests.len();
    let rerun = recover(&fixture, &server_url).await;
    assert!(rerun.status.success(), "{}", stderr_text(&rerun));
    assert_eq!(batches.lock().unwrap().len(), before);
    server.abort();
}

/// `--require-server` restores the fail-closed behavior: the old augmented
/// connect error (including "the agent was not started"), and the harness
/// never starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn require_server_fails_closed_when_offline() {
    let fixture = Fixture::new();
    let server = closed_port_url();
    let output = run(
        &fixture,
        &server,
        &["run", "--no-autowire", "--require-server", "claude"],
    )
    .await;
    assert!(!output.status.success());
    let output_stderr = stderr_text(&output);
    assert!(
        output_stderr.contains("the agent was not started"),
        "the prepare context is preserved:\n{output_stderr}"
    );
    assert!(
        output_stderr.contains("could not reach"),
        "the augmented connect diagnosis is reused:\n{output_stderr}"
    );
    assert!(
        !fixture.claude_ran.exists(),
        "the harness must not run:\n{output_stderr}"
    );
}

/// Offline auto-wire installs the hooks (capture must spool locally) but
/// registers no MCP entry for the unreachable server and leaves no sentinel,
/// so the next online launch completes the wiring.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_autowire_wires_hooks_but_not_mcp() {
    let fixture = Fixture::new();
    let server = closed_port_url();
    let output = run(&fixture, &server, &["run", "claude"]).await;
    assert!(output.status.success(), "{}", stderr_text(&output));
    let output_stderr = stderr_text(&output);
    assert!(
        output_stderr.contains("wiring its ai-memory hooks"),
        "the offline wiring is announced:\n{output_stderr}"
    );
    assert!(
        output_stderr.contains("MCP registration waits"),
        "the deferred MCP is announced:\n{output_stderr}"
    );

    let settings = fs::read_to_string(fixture.home.join(".claude/settings.json"))
        .expect("hooks settings written");
    assert!(
        settings.contains("ai-memory") || settings.contains("ai_memory"),
        "hooks are wired offline: {settings}"
    );
    // With CLAUDE_CONFIG_DIR set, the Claude Code MCP target is
    // `<config_dir>/.claude.json`; check both spellings so the assertion
    // cannot pass vacuously.
    for mcp_path in [
        fixture.home.join(".claude.json"),
        fixture.home.join(".claude/.claude.json"),
    ] {
        assert!(
            !mcp_path.exists() || !fs::read_to_string(&mcp_path).unwrap().contains("ai-memory"),
            "no dead-server MCP registration may be written at {}",
            mcp_path.display()
        );
    }
    let state = fixture.home.join("data/autowire-state");
    assert!(
        !state.exists() || fs::read_dir(&state).unwrap().count() == 0,
        "an offline half-wiring must not gate the next online launch"
    );
}

/// The mid-run outage repair path: when the server dies after prepare, the
/// child's exit code survives, the warning points at `recover`, and the
/// original run id stays journaled without repository-state details. The mock server answers
/// prepare, then closes its listener once the run is linked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mid_run_outage_preserves_the_child_exit_code() {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let trigger = shutdown_tx.clone();
    let app = Router::new()
        .route(
            "/workstream/runs",
            post(|| async {
                Json(json!({
                    "workstream_id": "12345678-1234-4234-9234-123456789abd",
                    "workstream_name": "fixture",
                    "run_id": "12345678-1234-4234-9234-123456789abe",
                    "resolved_agent": "claude-code",
                    "sync_after": 0, "sync_through": 0,
                    "may_adopt_existing_session": false,
                }))
            }),
        )
        .route(
            "/workstream/runs/{run_id}/link",
            post(move || {
                let _ = trigger.send(true);
                async { StatusCode::NO_CONTENT }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let mut rx = shutdown_rx;
                while rx.changed().await.is_ok() {
                    if *rx.borrow() {
                        break;
                    }
                }
            })
            .await
            .unwrap();
    });

    let fixture = Fixture::new();
    // Hold the child long enough for the graceful shutdown to close the
    // listener before the finish POSTs begin, then exit 5.
    write_script(
        &fixture.bin.join("claude"),
        &format!(
            "printf '%s\\n' \"$@\" > '{ran}'\n\
             while [ $# -gt 0 ]; do\n\
             if [ \"$1\" = --session-id ]; then\n\
             printf '{{\"sessionId\":\"%s\",\"cwd\":\"%s\"}}\\n{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"finish recovery prompt\"}}]}}}}\\n' \"$2\" '{repo}' > '{dir}'/\"$2\".jsonl\n\
             fi\nshift\ndone\nsleep 1\nexit 5\n",
            ran = fixture.claude_ran.display(),
            repo = fixture.repo.display(),
            dir = fixture
                .home
                .join(".claude/projects")
                .join(fixture.repo.to_string_lossy().replace('/', "-"))
                .display(),
        ),
    );
    let server_url = format!("http://{address}");
    let output = run(&fixture, &server_url, &["run", "--no-autowire", "claude"]).await;
    assert_eq!(
        output.status.code(),
        Some(5),
        "the child's exit code is preserved, not an ai-memory error:\n{}",
        stderr_text(&output)
    );
    let output_stderr = stderr_text(&output);
    assert!(
        output_stderr.contains("was not imported"),
        "the outage is reported:\n{output_stderr}"
    );
    assert!(
        output_stderr.contains("ai-memory recover"),
        "the automatic repair path is named:\n{output_stderr}"
    );
    let entries = journal_entries(&fixture);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "finish-failed");
    assert_eq!(entries[0]["run_id"], "12345678-1234-4234-9234-123456789abe");
    assert_eq!(entries[0]["exit_code"], 5);
    assert!(entries[0].get("checkpoint").is_none());
    assert!(entries[0].get("changed_paths").is_none());

    server.await.unwrap();
    let listener = tokio::net::TcpListener::bind(address).await.unwrap();
    let batches = Arc::new(Mutex::new(Vec::new()));
    let finishes = Arc::new(Mutex::new(Vec::new()));
    let recovery_server =
        fake_recovery_server_on(listener, batches.clone(), finishes.clone()).await;
    let recovered = recover(&fixture, &server_url).await;
    assert!(
        recovered.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&recovered.stdout),
        stderr_text(&recovered)
    );
    assert!(journal_entries(&fixture).is_empty());
    assert!(batches.lock().unwrap().is_empty());
    let finish_requests = finishes.lock().unwrap().clone();
    assert_eq!(finish_requests.len(), 1);
    assert_eq!(finish_requests[0].0, "12345678-1234-4234-9234-123456789abe");
    assert_eq!(finish_requests[0].1["complete"], true);
    assert_eq!(finish_requests[0].1["exit_code"], 5);
    assert!(finish_requests[0].1["checkpoint"].is_object());
    assert_eq!(
        finish_requests[0].1["events"][0]["content"],
        "finish recovery prompt"
    );

    let rerun = recover(&fixture, &server_url).await;
    assert!(rerun.status.success(), "{}", stderr_text(&rerun));
    assert_eq!(finishes.lock().unwrap().len(), 1);
    recovery_server.abort();
}
