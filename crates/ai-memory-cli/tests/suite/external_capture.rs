//! An external producer may replace capture without replacing native context delivery.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

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
        let tmp = tempfile::tempdir().unwrap().keep();
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
    let root = tempfile::tempdir().unwrap().keep();
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
    let tmp = tempfile::tempdir().unwrap().keep();
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
    assert!(!tmp.as_path().join("data").exists());
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
            let root = tempfile::tempdir().unwrap().keep();
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

    #[tokio::test]
    async fn external_capture_preserves_native_handoffs_and_mcp_recall() {
        let fixture = Fixture::start().await;
        for (agent, event) in [
            ("claude-code", "session-start"),
            ("kimi-code", "user-prompt-submit"),
        ] {
            fixture.seed_handoff().await;
            let receiver = SessionId::new();
            let output = fixture.native(receiver, agent, event);
            assert!(output.contains(MARKER), "handoff was lost: {output}");
            assert!(
                fixture
                    .reader
                    .observations_for_session(receiver)
                    .await
                    .unwrap()
                    .is_empty()
            );
            assert!(
                !fixture.native(receiver, agent, event).contains(MARKER),
                "handoff was delivered twice"
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
                    let response = client
                        .post(&endpoint)
                        .bearer_auth(TOKEN)
                        .json(&items)
                        .send()
                        .await
                        .unwrap();
                    assert!(response.status().is_success(), "{}", response.status());
                    let ack: Value = response.json().await.unwrap();
                    assert_eq!(ack["accepted"], 3, "{ack}");
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
