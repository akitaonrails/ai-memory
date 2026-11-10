//! An external producer may replace capture without replacing native context delivery.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::e2e_support::hermetic;

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");

fn hook(root: &Path, agent: &str, event: &str, owner: Option<&str>, check: bool) -> Output {
    let mut cmd = hermetic(BIN);
    cmd.args(["--data-dir"])
        .arg(root.join("data"))
        .args(["hook", "--agent", agent, "--event", event, "--server-url"])
        .arg(ai_memory_test_support::dead_http_endpoint())
        .env("AI_MEMORY_HOME", root)
        .env("HOME", root)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(owner) = owner {
        cmd.env("AI_MEMORY_CAPTURE_OWNER", owner);
    }
    if check {
        cmd.arg("--check-capture");
    }
    let mut child = cmd.spawn().expect("spawn hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({"session_id":"external-owner-test", "cwd":root, "prompt":"capture marker"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "{result:?}");
    result
}

#[test]
fn external_capture_owner_suppresses_only_the_opted_in_process() {
    for owner in [None, Some(""), Some(" \t\n"), Some("orchestrator-a")] {
        let guard = tempfile::tempdir().unwrap();
        let tmp = guard.path().to_path_buf();
        let output = hook(
            tmp.as_path(),
            "claude-code",
            "user-prompt-submit",
            owner,
            false,
        );
        assert_eq!(output.stdout, b"{}\n");
        let spool = tmp.as_path().join("data/hook-spool");
        let entries = std::fs::read_dir(spool)
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(
            entries,
            usize::from(owner != Some("orchestrator-a")),
            "owner={owner:?}"
        );
    }
}

#[test]
fn external_devin_end_still_clears_native_identity() {
    let guard = tempfile::tempdir().unwrap();
    let root = guard.path().to_path_buf();
    let state = root.join("data/hook-state/devin-session-id");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    std::fs::write(&state, "completed-execution").unwrap();

    hook(&root, "devin", "session-end", Some("orchestrator-a"), true);
    assert!(state.exists(), "inspection must not clear identity");
    hook(&root, "devin", "session-end", Some("orchestrator-a"), false);
    assert!(
        !state.exists(),
        "SessionEnd must retire native fallback identity"
    );
    assert!(!root.join("data/hook-spool").exists());
}

#[test]
fn capture_inspection_reports_external_ownership_without_side_effects() {
    let guard = tempfile::tempdir().unwrap();
    let tmp = guard.path().to_path_buf();
    std::fs::write(
        tmp.join(".ai-memory.toml"),
        "workspace = \"demo\"\nproject = \"app\"\n",
    )
    .unwrap();
    let output = hook(
        tmp.as_path(),
        "claude-code",
        "user-prompt-submit",
        Some("orchestrator-a"),
        true,
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["external_capture"], true);
    assert_eq!(report["admits_capture"], false);
    assert_eq!(report["policy_admits_capture"], true);
    assert_eq!(report["scope"]["workspace"], "demo");
    assert_eq!(report["scope"]["project"], "app");
    assert_eq!(report["scope_resolution"], "explicit");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("orchestrator-a"));
    assert!(!tmp.as_path().join("data").exists());
}

/// Runs one `user-prompt-submit` with `prompt` against `server`, owned
/// externally throughout.
fn external_prompt(root: &Path, server: &str, prompt: &str) -> Output {
    let mut child = hermetic(BIN)
        .args(["--data-dir"])
        .arg(root.join("data"))
        .args([
            "hook",
            "--agent",
            "claude-code",
            "--event",
            "user-prompt-submit",
            "--server-url",
            server,
        ])
        .env("AI_MEMORY_HOME", root)
        .env("HOME", root)
        .env("AI_MEMORY_CAPTURE_OWNER", "example.runtime")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({"session_id":"external-recap-test", "cwd":root, "prompt":prompt})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success(), "{result:?}");
    result
}

/// A one-shot `GET` stub that answers with `body`. It stops on the returned
/// flag rather than on a timeout, so a run that issues no request at all can
/// be asserted on without waiting one out.
fn serve_one_request(
    body: &'static str,
) -> (
    String,
    Arc<AtomicBool>,
    std::thread::JoinHandle<Option<String>>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let served = std::thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket.set_nonblocking(false).unwrap();
                    socket
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        if socket.read_exact(&mut byte).is_err() {
                            break;
                        }
                        request.push(byte[0]);
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes());
                    let _ = socket.shutdown(std::net::Shutdown::Write);
                    return Some(String::from_utf8_lossy(&request).into_owned());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if flag.load(Ordering::SeqCst) {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("accepting the stub request: {error}"),
            }
        }
    });
    (format!("http://{address}"), stop, served)
}

/// Delivering the compaction recap is context delivery, like `SessionStart`'s
/// handoff, so an external capture owner must not suppress it: the recap
/// reaches the model through the `UserPromptSubmit` envelope for every agent
/// whose `user_prompt_injects_context` is true. The exemption stays scoped to
/// that delivery — an ordinary prompt under the same owner still early-returns
/// without touching the server — and it leaks no capture, because every
/// enqueue and drain on the path is separately gated on `!external_capture`.
#[test]
fn external_capture_still_delivers_the_compaction_recap() {
    const RECAP: &str = "RECAP_SENTINEL";
    for (prompt, delivers) in [("reload context", true), ("an ordinary prompt", false)] {
        let guard = tempfile::tempdir().unwrap();
        let root = guard.path().to_path_buf();
        let (server, stop, served) = serve_one_request(RECAP);

        let output = external_prompt(&root, &server, prompt);

        stop.store(true, Ordering::SeqCst);
        let request = served.join().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if delivers {
            let request = request.unwrap_or_else(|| {
                panic!("the recap GET never reached the server; stdout was {stdout:?}")
            });
            assert!(request.starts_with("GET /recap?"), "{request}");
            let envelope: Value = serde_json::from_str(stdout.trim()).unwrap();
            assert_eq!(
                envelope["hookSpecificOutput"]["hookEventName"],
                "UserPromptSubmit"
            );
            assert_eq!(envelope["hookSpecificOutput"]["additionalContext"], RECAP);
        } else {
            assert_eq!(request, None, "{prompt:?} must not request a recap");
            assert_eq!(stdout, "{}\n", "{prompt:?} must early-return");
        }
        let spooled = std::fs::read_dir(root.join("data/hook-spool"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(spooled, 0, "{prompt:?} must leave capture to its owner");
    }
}

/// The subagent brief is context delivery too: under an external capture
/// owner, a repository that opted into `[briefing] inject_on_subagent_start`
/// still hands the subagent its brief, and the event itself is
/// left to the owner.
#[test]
fn external_capture_still_delivers_the_subagent_brief() {
    const BRIEF: &str = "SUBAGENT_BRIEF_SENTINEL";
    let guard = tempfile::tempdir().unwrap();
    let root = guard.path().to_path_buf();
    std::fs::write(
        root.join(".ai-memory.toml"),
        "[briefing]\ninject_on_subagent_start = true\n",
    )
    .unwrap();
    let (server, stop, served) = serve_one_request(BRIEF);

    let mut child = hermetic(BIN)
        .args(["--data-dir"])
        .arg(root.join("data"))
        .args([
            "hook",
            "--agent",
            "claude-code",
            "--event",
            "subagent-start",
            "--server-url",
            &server,
        ])
        .env("AI_MEMORY_HOME", &root)
        .env("HOME", &root)
        .env("AI_MEMORY_CAPTURE_OWNER", "example.runtime")
        .current_dir(&root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            json!({
                "session_id": "external-subagent-test",
                "cwd": root,
                "hook_event_name": "SubagentStart",
                "agent_id": "agent-1",
                "agent_type": "Explore"
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");

    stop.store(true, Ordering::SeqCst);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let request = served
        .join()
        .unwrap()
        .unwrap_or_else(|| panic!("the brief GET never reached the server; stdout was {stdout:?}"));
    assert!(request.starts_with("GET /brief?"), "{request}");
    let envelope: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(
        envelope["hookSpecificOutput"]["hookEventName"],
        "SubagentStart"
    );
    assert_eq!(envelope["hookSpecificOutput"]["additionalContext"], BRIEF);
    let spooled = std::fs::read_dir(root.join("data/hook-spool"))
        .map(|entries| entries.flatten().count())
        .unwrap_or(0);
    assert_eq!(spooled, 0, "the event must be left to its capture owner");
}

mod slow {
    use super::*;
    use std::process::Command;
    use std::time::Duration;

    use ai_memory_core::SessionId;
    use ai_memory_store::ReaderPool;
    use sha2::{Digest, Sha256};

    use crate::e2e_support::{ServerGuard, start_serve};

    const TOKEN: &str = "external-capture-local-test-token";
    const MARKER: &str = "external_capture_handoff_marker";

    struct Fixture {
        _server: ServerGuard,
        reader: ReaderPool,
        root: PathBuf,
        client: reqwest::Client,
        base: String,
        // Last field: drops after the server has stopped using the tree.
        _guard: tempfile::TempDir,
    }

    fn command(root: &Path) -> Command {
        let mut cmd = hermetic(BIN);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        cmd.env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", root.join("empty-gitconfig"))
            .env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"))
            .env("AI_MEMORY_HOME", root.join("home"))
            .env("AI_MEMORY_DATA_DIR", root.join("data"))
            .env("AI_MEMORY_AUTH_TOKEN", TOKEN)
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
            .env("AI_MEMORY_BACKFILL_ON_START", "false")
            .env("AI_MEMORY_CONSOLIDATE_ON_SESSION_END", "false")
            .env("AI_MEMORY_AUTO_IMPROVE__SCHEDULER__ENABLED", "false")
            .env("RUST_LOG", "off")
            .current_dir(root.join("project"));
        cmd
    }

    impl Fixture {
        async fn start() -> Self {
            let guard = tempfile::tempdir().unwrap();
            let root = guard.path().to_path_buf();
            for dir in ["home", "data", "project"] {
                std::fs::create_dir(root.as_path().join(dir)).unwrap();
            }
            std::fs::write(root.as_path().join("empty-gitconfig"), "").unwrap();
            std::fs::write(
                root.as_path().join("project/.ai-memory.toml"),
                "workspace = \"external-test\"\nproject = \"capture-test\"\n",
            )
            .unwrap();
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap();
            let (server, base) = start_serve(&client, &root.as_path().join("serve.log"), |port| {
                let mut cmd = command(root.as_path());
                cmd.args([
                    "serve",
                    "--transport",
                    "http",
                    "--bind",
                    &format!("127.0.0.1:{port}"),
                    "--workspace",
                    "external-test",
                    "--project",
                    "capture-test",
                    "--no-watcher",
                ]);
                cmd
            })
            .await;
            let reader = ReaderPool::new(&root.as_path().join("data/db/memory.sqlite"), 2).unwrap();
            Self {
                _server: server,
                reader,
                root,
                client,
                base,
                _guard: guard,
            }
        }

        fn event(&self, sid: SessionId, producer: &str, event_id: &str, event: &str) -> Value {
            // An adapter-side convention: hash stable identity, never message content.
            let identity = json!([
                "external-capture-v1",
                producer,
                "operator-a",
                "claude-code",
                sid,
                event,
                event_id
            ]);
            let key = format!("{:x}", Sha256::digest(identity.to_string().as_bytes()));
            let mut url = reqwest::Url::parse(&format!("{}/hook", self.base)).unwrap();
            url.query_pairs_mut().extend_pairs([
                ("event", event),
                ("agent", "claude-code"),
                ("workspace", "external-test"),
                ("project", "capture-test"),
                ("extension", producer),
                ("source_event", event),
                ("ingest_key", &key),
            ]);
            json!({"url":url.as_str(), "body":{"session_id":sid, "cwd":self.root.as_path().join("project"), "prompt":MARKER}})
        }

        async fn batch(&self, items: &[Value]) {
            let response = self
                .client
                .post(format!("{}/hook/batch", self.base))
                .bearer_auth(TOKEN)
                .json(items)
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success(), "{}", response.status());
            let ack: Value = response.json().await.unwrap();
            assert_eq!(ack["accepted"], items.len(), "{ack}");
        }

        async fn seed_handoff(&self) {
            let sid = SessionId::new();
            self.batch(&[
                self.event(sid, "orchestrator-a", "start", "session-start"),
                self.event(sid, "orchestrator-a", "prompt", "user-prompt-submit"),
                self.event(sid, "orchestrator-a", "end", "session-end"),
            ])
            .await;
        }

        fn native(&self, sid: SessionId, agent: &str, event: &str) -> String {
            let mut child = command(self.root.as_path())
                .args([
                    "hook",
                    "--agent",
                    agent,
                    "--event",
                    event,
                    "--server-url",
                    &self.base,
                    "--auth-token",
                    TOKEN,
                ])
                .env("AI_MEMORY_CAPTURE_OWNER", "orchestrator-a")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(
                json!({"session_id":sid,"cwd":self.root.as_path().join("project"),"prompt":"native duplicate"})
                    .to_string().as_bytes(),
            ).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(output.stderr.is_empty(), "{output:?}");
            String::from_utf8(output.stdout).unwrap()
        }
    }

    impl Fixture {
        /// A CodeBuddy SessionStart, run from inside the project, whose payload
        /// carries `cwd` only when given one.
        fn codebuddy_start(&self, cwd: Option<&Path>, project_dir: Option<&Path>) -> Output {
            let mut cmd = command(self.root.as_path());
            cmd.args([
                "hook",
                "--agent",
                "codebuddy",
                "--event",
                "session-start",
                "--server-url",
                &self.base,
                "--auth-token",
                TOKEN,
            ])
            .env_remove("CODEBUDDY_PROJECT_DIR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
            if let Some(dir) = project_dir {
                cmd.env("CODEBUDDY_PROJECT_DIR", dir);
            }
            let mut body = json!({"session_id": SessionId::new(), "source": "startup"});
            if let Some(cwd) = cwd {
                body["cwd"] = json!(cwd);
            }
            let mut child = cmd.spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(body.to_string().as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            output
        }
    }

    /// Security boundary 6c: a SessionStart that names no directory must not
    /// claim a handoff. The server would resolve such a claim to its default
    /// project, here the one holding the baton, so an agent opened anywhere
    /// else would consume it. The hook process runs inside that project on
    /// purpose: its own directory is not evidence of where CodeBuddy is.
    /// Controls: the payload's `cwd`, or `CODEBUDDY_PROJECT_DIR`, claims it.
    #[tokio::test]
    async fn codebuddy_start_without_a_directory_leaves_the_default_projects_handoff() {
        let fixture = Fixture::start().await;
        let project = fixture.root.as_path().join("project");
        fixture.seed_handoff().await;

        let unscoped = fixture.codebuddy_start(None, None);
        let stdout = String::from_utf8_lossy(&unscoped.stdout);
        assert!(!stdout.contains(MARKER), "unscoped claim: {stdout}");
        assert!(
            String::from_utf8_lossy(&unscoped.stderr).contains("memory_handoff_accept"),
            "{unscoped:?}"
        );

        let scoped = fixture.codebuddy_start(Some(&project), None);
        let stdout = String::from_utf8_lossy(&scoped.stdout);
        assert!(
            stdout.contains(MARKER),
            "the handoff must still be open for its own project: {stdout}"
        );
        assert!(stdout.contains("additionalContext"), "{stdout}");

        fixture.seed_handoff().await;
        let from_env = fixture.codebuddy_start(None, Some(&project));
        assert!(
            String::from_utf8_lossy(&from_env.stdout).contains(MARKER),
            "{from_env:?}"
        );
    }

    /// Under external capture the owner's own runs share the project with the
    /// captured harness, so a pending handoff is *offered*, never consumed: the
    /// start announces the baton by id with `memory_handoff_accept` and leaves
    /// it open for an explicit claim, rather than delivering (and consuming) its
    /// body as a claim would. Native capture stays suppressed and MCP recall
    /// still finds the seeded content (#2148).
    #[tokio::test]
    async fn external_capture_offers_handoffs_without_consuming_them_and_keeps_mcp_recall() {
        let fixture = Fixture::start().await;
        for (agent, event) in [
            ("claude-code", "session-start"),
            ("kimi-code", "user-prompt-submit"),
        ] {
            fixture.seed_handoff().await;
            let receiver = SessionId::new();
            let offered = fixture.native(receiver, agent, event);
            assert!(
                offered.contains("memory_handoff_accept") && !offered.contains(MARKER),
                "external capture must offer the baton, not deliver its body: {offered}"
            );
            assert!(
                fixture
                    .reader
                    .observations_for_session(receiver)
                    .await
                    .unwrap()
                    .is_empty(),
                "external capture must still suppress native capture"
            );
            // Offer mode never consumes the baton: a fresh start still sees it.
            let next = SessionId::new();
            assert!(
                fixture
                    .native(next, agent, event)
                    .contains("memory_handoff_accept"),
                "an offered handoff must stay open for the next start"
            );
            fixture.native(receiver, agent, "session-end");
            assert!(!fixture.root.as_path().join("data/hook-spool").exists());
            assert!(!fixture.root.as_path().join("data/backfill-state").exists());
        }
        let response: Value = fixture.client.post(format!("{}/mcp",fixture.base))
            .bearer_auth(TOKEN).header("accept","application/json, text/event-stream")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_query","arguments":{"query":MARKER,"workspace":"external-test","project":"capture-test"}}}))
            .send().await.unwrap().json().await.unwrap();
        assert!(response.get("error").is_none(), "{response}");
        assert_ne!(
            response.pointer("/result/isError"),
            Some(&Value::Bool(true)),
            "{response}"
        );
        assert!(
            response.to_string().contains(MARKER),
            "MCP did not recall external capture: {response}"
        );
    }

    #[tokio::test]
    async fn external_identities_retry_concurrently_without_collapsing_distinct_events() {
        let fixture = Fixture::start().await;
        let sid = SessionId::new();
        let first = fixture.event(sid, "orchestrator-a", "prompt-1", "user-prompt-submit");
        let unauthorized = fixture
            .client
            .post(format!("{}/hook/batch", fixture.base))
            .json(std::slice::from_ref(&first))
            .send()
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
        let retry = [first];
        tokio::join!(fixture.batch(&retry), fixture.batch(&retry));
        fixture
            .batch(&[
                fixture.event(sid, "orchestrator-a", "prompt-2", "user-prompt-submit"),
                fixture.event(sid, "orchestrator-b", "prompt-1", "user-prompt-submit"),
            ])
            .await;
        let observations = fixture.reader.observations_for_session(sid).await.unwrap();
        assert_eq!(observations.len(), 3);
        assert!(
            observations.iter().all(
                |o| o.body == MARKER && o.source_event.as_deref() == Some("user-prompt-submit")
            )
        );
        assert_eq!(
            observations
                .iter()
                .filter(|o| o.extension.as_deref() == Some("orchestrator-b"))
                .count(),
            1
        );

        // A project-scoped key must not consume an event in another workspace.
        let other_sid = SessionId::new();
        let mut other = retry[0].clone();
        other["body"]["session_id"] = json!(other_sid);
        other["url"] = Value::String(
            other["url"]
                .as_str()
                .unwrap()
                .replace("workspace=external-test", "workspace=other-workspace"),
        );
        fixture.batch(&[other]).await;
        assert_eq!(
            fixture
                .reader
                .observations_for_session(other_sid)
                .await
                .unwrap()
                .len(),
            1
        );
        let original_scope = fixture
            .reader
            .find_session_scope(sid)
            .await
            .unwrap()
            .unwrap();
        let other_scope = fixture
            .reader
            .find_session_scope(other_sid)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(original_scope.0, other_scope.0);
        assert_ne!(original_scope.1, other_scope.1);
    }

    #[tokio::test]
    async fn documented_batch_example_is_idempotent() {
        let fixture = Fixture::start().await;
        let doc = include_str!("../../../../docs/external-lifecycle.md").replace("\r\n", "\n");
        // Git may check out Markdown with CRLF on Windows. Exercise both
        // forms here so the example remains executable on either platform.
        for doc in [doc.clone(), doc.replace('\n', "\r\n")] {
            let example = doc
                .split("```json")
                .nth(1)
                .unwrap()
                .split("```")
                .next()
                .unwrap();
            let events: Vec<Value> = serde_json::from_str(example).unwrap();
            fixture.batch(&events).await;
            fixture.batch(&events).await;
        }
        assert_eq!(
            fixture.reader.status_counts().await.unwrap().observations,
            1
        );
    }

    /// Post a batch the way a real spool drain does when the shared server is
    /// saturated: a 429 (ingest shed) or a 200 with `failed_index` (fail-fast
    /// on a transient processing error — the SessionEnd item writes the
    /// session page, git checkpoint, and handoff inline, inside the response
    /// window) acknowledges exactly the items that committed, so resend only
    /// the rest, bounded. Ingest keys make resending an item an earlier
    /// attempt did accept a replay, so the drain still converges to
    /// exactly-once.
    async fn post_batch_until_accepted(client: &reqwest::Client, endpoint: &str, items: &[Value]) {
        let mut pending = items.to_vec();
        let mut ack = Value::Null;
        for attempt in 0..20u64 {
            // A shared server saturated by every session draining at once can
            // take longer than the client request timeout to answer a single
            // batch. The real spool drain treats a transient timeout as a
            // retryable attempt and resends, so model the same bounded backoff
            // here instead of failing the whole batch on one slow response.
            let response = match client
                .post(endpoint)
                .bearer_auth(TOKEN)
                .json(&pending)
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) if err.is_timeout() => {
                    tokio::time::sleep(Duration::from_millis(20 * (attempt + 1))).await;
                    continue;
                }
                Err(err) => panic!("unexpected transport error posting batch: {err}"),
            };
            let status = response.status();
            ack = match response.json().await {
                Ok(ack) => ack,
                Err(err) if err.is_timeout() => {
                    tokio::time::sleep(Duration::from_millis(20 * (attempt + 1))).await;
                    continue;
                }
                Err(err) => panic!("unexpected error reading batch response: {err}"),
            };
            assert!(
                status.is_success() || status == reqwest::StatusCode::TOO_MANY_REQUESTS,
                "{status} {ack}"
            );
            // `accepted_indices` is present exactly when the acknowledged
            // items are not the contiguous leading prefix, where the legacy
            // `accepted` count would under-report.
            let accepted: Vec<u64> = match ack["accepted_indices"].as_array() {
                Some(indices) => indices.iter().filter_map(Value::as_u64).collect(),
                None => (0..ack["accepted"].as_u64().unwrap()).collect(),
            };
            if accepted.len() == pending.len() {
                return;
            }
            pending = pending
                .iter()
                .enumerate()
                .filter(|(idx, _)| !accepted.contains(&(*idx as u64)))
                .map(|(_, item)| item.clone())
                .collect();
            tokio::time::sleep(Duration::from_millis(20 * (attempt + 1))).await;
        }
        panic!("batch never fully accepted after 20 attempts: {ack}");
    }

    #[tokio::test]
    async fn fifteen_external_sessions_share_one_server_and_retry_completed_batches() {
        let fixture = Fixture::start().await;
        let mut tasks = tokio::task::JoinSet::new();
        for index in 0..15 {
            let sid = SessionId::new();
            let producer = format!("runtime-{}", index % 3);
            let items = vec![
                fixture.event(sid, &producer, "1", "session-start"),
                fixture.event(sid, &producer, "2", "user-prompt-submit"),
                fixture.event(sid, &producer, "3", "session-end"),
            ];
            let client = fixture.client.clone();
            let endpoint = format!("{}/hook/batch", fixture.base);
            tasks.spawn(async move {
                for _ in 0..2 {
                    post_batch_until_accepted(&client, &endpoint, &items).await;
                }
                sid
            });
        }
        while let Some(result) = tasks.join_next().await {
            let observations = fixture
                .reader
                .observations_for_session(result.unwrap())
                .await
                .unwrap();
            assert_eq!(observations.len(), 3);
        }
        assert_eq!(
            fixture.reader.status_counts().await.unwrap().observations,
            45
        );
    }
}
