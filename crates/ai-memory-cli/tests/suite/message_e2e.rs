//! Use-case end-to-end test for `ai-memory message` (cross-project inbox/queue).
//!
//! Cross-project messaging lets an agent in project A drop a self-contained
//! request into project B's inbox without pulling B's context into A's session;
//! a session in B later pops it exactly once. The store layer is pinned in
//! `ai-memory-store`'s `agent_messages.rs` and the MCP tools in
//! `ai-memory-mcp`'s `agent_messages_tools.rs`. The UNTESTED half is the whole
//! CLI surface (`crates/ai-memory-cli/src/commands/message.rs` had zero tests):
//! does the shipped `ai-memory message send|list|pop|cancel`, driven exactly as
//! an operator would against a real spawned server, actually round-trip a
//! message across the isolation boundary and enforce claim-once + recipient-only
//! visibility + sender-only cancel?
//!
//! This drives that flow against a live `ai-memory serve`:
//!
//! 1. **Send + recipient-only visibility.** A→B send shows up in B's inbox and
//!    A's outbox, and in neither A's inbox nor an unrelated project's inbox.
//! 2. **Pop claims exactly once.** B pops it (body fenced as untrusted input,
//!    with the security notice); a second pop finds nothing.
//! 3. **Sender cancel.** A second A→B send, retracted from A by id, leaves B's
//!    inbox empty — the recipient never sees it.
//!
//! The recipient project must already exist for a send to be accepted (the
//! server resolves both scopes with a no-create lookup), so the test first makes
//! both projects real by firing a `session-start` hook for each — the "an agent
//! must have run there at least once" precondition from `docs/agent-messaging.md`.

/// Spawns a server and several subprocesses: seconds, not milliseconds, so this
/// lives in the slow tier (`cargo tf` / CI), not the everyday loop.
mod slow {
    use std::path::Path;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use crate::e2e_support::{ServerGuard, free_port, hermetic, run_cli, session_count};

    const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
    const WORKSPACE: &str = "message-e2e-ws";
    /// The sender project (the baked "current project" of the spawned server).
    const SENDER: &str = "message-e2e-a";
    /// The recipient project the sender addresses.
    const RECIPIENT: &str = "message-e2e-b";
    /// A third, uninvolved project — it must never see A→B mail.
    const BYSTANDER: &str = "message-e2e-c";

    /// Fire one `session-start` hook for `project`, then wait until the server
    /// has committed it — which both creates the project scope (so a later send
    /// can resolve it with the no-create lookup) and is the docs' "an agent ran
    /// here at least once" precondition.
    async fn ensure_project(client: &reqwest::Client, base: &str, project: &str, nth: u8) {
        // A unique session id per project: a shared id would make the later
        // hooks look like a cross-project session collision and be dropped,
        // leaving those projects uncreated.
        let session_id = format!("00000000-0000-4000-8000-0000000000{nth:02x}");
        let resp = client
            .post(format!("{base}/hook"))
            .query(&[
                ("event", "session-start"),
                ("agent", "claude-code"),
                ("workspace", WORKSPACE),
                ("project", project),
            ])
            .header("content-type", "application/json")
            .body(
                json!({
                    "event": "session-start",
                    "session_id": session_id,
                    "cwd": format!("/tmp/message-e2e/{project}"),
                })
                .to_string(),
            )
            .send()
            .await
            .expect("hook request");
        assert!(
            resp.status().is_success() || resp.status().as_u16() == 202,
            "hook must be accepted for {project}: {}",
            resp.status()
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if session_count(client, base, WORKSPACE, project).await >= 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "project {project} was never created by its session-start hook"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `ai-memory message send` A→B with `--json`, returning the new message id.
    fn send(data_dir: &Path, home: &Path, base: &str, subject: &str, body: &str) -> String {
        let out = run_cli(
            &[
                "message",
                "send",
                "--from-workspace",
                WORKSPACE,
                "--from-project",
                SENDER,
                "--to-workspace",
                WORKSPACE,
                "--to-project",
                RECIPIENT,
                "--subject",
                subject,
                "--json",
                body,
            ],
            data_dir,
            home,
            None,
            base,
        );
        let report: Value = serde_json::from_str(&out).expect("send --json report");
        report["message_id"]
            .as_str()
            .unwrap_or_else(|| panic!("send returned no message_id: {report}"))
            .to_string()
    }

    /// `ai-memory message list` for `project` with `--json`, returning the array.
    fn list(data_dir: &Path, home: &Path, base: &str, project: &str, outbox: bool) -> Vec<Value> {
        let mut args = vec![
            "message",
            "list",
            "--workspace",
            WORKSPACE,
            "--project",
            project,
            "--json",
        ];
        if outbox {
            args.push("--outbox");
        }
        let out = run_cli(&args, data_dir, home, None, base);
        serde_json::from_str(&out).expect("list --json array")
    }

    #[tokio::test]
    async fn message_round_trips_across_projects_with_claim_once_and_cancel() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");

        // Start the real server (loopback, no auth, hermetic: no embedder, no
        // wiki watcher — a machine-global inotify instance concurrent server
        // children can exhaust, #745).
        let port = free_port();
        let base = format!("http://127.0.0.1:{port}");
        let server = ServerGuard(
            hermetic(BIN)
                .args([
                    "serve",
                    "--transport",
                    "http",
                    "--bind",
                    &format!("127.0.0.1:{port}"),
                    "--workspace",
                    WORKSPACE,
                    "--project",
                    SENDER,
                    "--no-watcher",
                ])
                .env("AI_MEMORY_DATA_DIR", data_dir.path())
                .env("AI_MEMORY_HOME", home.path())
                .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
                .env("RUST_LOG", "off")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn serve"),
        );

        let client = reqwest::Client::new();

        // Wait for the server to accept connections.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if client
                .get(format!("{base}/mcp"))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            assert!(Instant::now() < deadline, "server never became reachable");
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        // A send is rejected unless the recipient project already exists, so make
        // all three projects real by "running an agent" in each once.
        for (nth, project) in [SENDER, RECIPIENT, BYSTANDER].into_iter().enumerate() {
            ensure_project(&client, &base, project, nth as u8 + 1).await;
        }

        // Phase 1 — send A→B, then check where it is visible.
        let body = "Add a /v1/export endpoint that streams NDJSON. Keep the auth middleware.";
        let id = send(data_dir.path(), home.path(), &base, "export endpoint", body);

        let inbox_b = list(data_dir.path(), home.path(), &base, RECIPIENT, false);
        assert_eq!(
            inbox_b.len(),
            1,
            "B's inbox must show exactly the sent message: {inbox_b:?}"
        );
        assert_eq!(inbox_b[0]["id"].as_str(), Some(id.as_str()));
        assert_eq!(inbox_b[0]["body"].as_str(), Some(body));
        assert_eq!(inbox_b[0]["subject"].as_str(), Some("export endpoint"));
        assert_eq!(inbox_b[0]["state"].as_str(), Some("pending"));

        // Recipient-only visibility: not in A's inbox, not in the bystander's.
        assert!(
            list(data_dir.path(), home.path(), &base, SENDER, false).is_empty(),
            "the sender must not see its outbound mail in its own inbox"
        );
        assert!(
            list(data_dir.path(), home.path(), &base, BYSTANDER, false).is_empty(),
            "an unrelated project must not see mail addressed to B"
        );

        // The sender sees it in its OUTBOX (what it can still cancel).
        let outbox_a = list(data_dir.path(), home.path(), &base, SENDER, true);
        assert_eq!(
            outbox_a.len(),
            1,
            "A's outbox must show the message it sent"
        );
        assert_eq!(outbox_a[0]["id"].as_str(), Some(id.as_str()));

        // Phase 2 — B pops it exactly once. The human rendering fences the body
        // as untrusted cross-project input and prints the security notice.
        let popped_human = run_cli(
            &[
                "message",
                "pop",
                "--workspace",
                WORKSPACE,
                "--project",
                RECIPIENT,
            ],
            data_dir.path(),
            home.path(),
            None,
            &base,
        );
        assert!(
            popped_human.contains(&id),
            "pop must name the popped message id: {popped_human}"
        );
        assert!(
            popped_human.contains(body),
            "pop must print the message body: {popped_human}"
        );
        assert!(
            popped_human.contains("untrusted cross-project input"),
            "pop must fence the body as untrusted input: {popped_human}"
        );

        // Claim-once: the inbox is now empty and a second pop returns nothing.
        assert!(
            list(data_dir.path(), home.path(), &base, RECIPIENT, false).is_empty(),
            "the inbox must be empty once the message is claimed"
        );
        let second_pop = run_cli(
            &[
                "message",
                "pop",
                "--workspace",
                WORKSPACE,
                "--project",
                RECIPIENT,
                "--json",
            ],
            data_dir.path(),
            home.path(),
            None,
            &base,
        );
        let second: Value = serde_json::from_str(&second_pop).expect("second pop --json");
        assert_eq!(
            second["message"],
            Value::Null,
            "a claimed message must not be poppable again: {second}"
        );

        // Phase 3 — sender cancel. A second send, retracted from A by id, must
        // never reach B.
        let id2 = send(
            data_dir.path(),
            home.path(),
            &base,
            "never mind",
            "cancel me",
        );
        assert_eq!(
            list(data_dir.path(), home.path(), &base, RECIPIENT, false).len(),
            1,
            "the second message must be pending in B before cancel"
        );
        let cancel = run_cli(
            &[
                "message",
                "cancel",
                "--workspace",
                WORKSPACE,
                "--project",
                SENDER,
                "--id",
                &id2,
                "--json",
            ],
            data_dir.path(),
            home.path(),
            None,
            &base,
        );
        let cancel: Value = serde_json::from_str(&cancel).expect("cancel --json");
        assert_eq!(
            cancel["cancelled"].as_u64(),
            Some(1),
            "the sender must cancel exactly its one pending message: {cancel}"
        );
        assert!(
            list(data_dir.path(), home.path(), &base, RECIPIENT, false).is_empty(),
            "a cancelled message must never reach the recipient inbox"
        );

        drop(server);
    }
}
