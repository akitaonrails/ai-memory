//! Use-case end-to-end test for `ai-memory doctor` (capture-coverage check).
//!
//! Doctor exists to make one silent failure visible: a harness that ran in this
//! project but has no ai-memory hook installed still writes its own local
//! session transcripts, yet nothing reaches the server. The operator believes
//! every harness feeds one memory and is quietly wrong (audit finding F1). For
//! the current project doctor enumerates each known harness's local native
//! sessions, asks the server how many it actually captured per agent
//! (`GET /admin/sessions/by-agent`), and warns — with the exact
//! `install-hooks --agent <agent> --apply` remediation — about any harness that
//! ran here recently but captured zero.
//!
//! The pure verdict logic (`build_rows`) and the local detector (`scan_local`)
//! are unit-tested in the command module. The UNTESTED half is the whole live
//! flow against a real server: does the shipped `doctor` subcommand, driven
//! exactly as an operator would, actually reconcile the local side against the
//! server's captured counts and print the right verdict? This drives that flow:
//!
//! 1. **Gap detected.** A planted Claude transcript for this cwd with an empty
//!    store (server captured zero) → doctor flags `claude-code` as uncaptured
//!    and prints its `install-hooks --agent claude-code --apply` fix.
//! 2. **Healthy control.** After `backfill` imports that same transcript through
//!    `/hook`, the server has captured sessions for `claude-code` → doctor no
//!    longer flags it, and reports every harness as captured. This control is
//!    what stops a blanket "everything is uncaptured" warning from passing #1
//!    for the wrong reason.
//!
//! Unix-gated: the fixture uses the POSIX-encoded `~/.claude/projects/<enc-cwd>/`
//! native layout (as the sibling `backfill_e2e` test and the `scan_local` unit
//! test do); cross-platform native-store discovery is owned by
//! `ai-memory-workstream`.

#![cfg(unix)]

/// Spawns a server and several subprocesses: seconds, not milliseconds, so this
/// lives in the slow tier (`cargo tf` / CI), not the everyday loop.
mod slow {
    use std::fs;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use crate::e2e_support::{
        ServerGuard, free_port, hermetic, run_cli, session_count, write_jsonl,
    };

    const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
    const WORKSPACE: &str = "doctor-e2e-ws";
    const PROJECT: &str = "doctor-e2e-proj";

    /// The row for one agent kind in a `doctor --json` report, or `None`.
    fn find_row<'a>(report: &'a Value, agent: &str) -> Option<&'a Value> {
        report["rows"]
            .as_array()?
            .iter()
            .find(|r| r["agent"] == agent)
    }

    #[tokio::test]
    async fn doctor_flags_an_uncaptured_harness_then_clears_it_after_backfill() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let project = tempfile::tempdir().expect("project cwd");
        // The child's `std::env::current_dir()` returns the canonical path, and
        // the native-session discovery matches the transcript's `cwd` header
        // against it — so plant and encode with the same canonical path.
        let cwd = fs::canonicalize(project.path()).expect("canonicalize project cwd");

        // Plant a Claude transcript for this cwd under the native layout. This
        // makes `claude-code` a harness that "ran here recently" for doctor,
        // while the server starts with zero captured — the F1 gap.
        let session_id = "11111111-2222-3333-4444-555555555555";
        let encoded = cwd.to_string_lossy().replace('/', "-");
        let session_dir = home.path().join(".claude").join("projects").join(encoded);
        fs::create_dir_all(&session_dir).expect("session dir");
        write_jsonl(
            &session_dir.join(format!("{session_id}.jsonl")),
            &[
                json!({ "sessionId": session_id, "cwd": cwd.to_string_lossy() }),
                json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{ "type": "text", "text": "Record the doctor-e2e decision." }],
                    },
                }),
                json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [{ "type": "text", "text": "Acknowledged the doctor-e2e plan." }],
                    },
                }),
            ],
        );

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
                    PROJECT,
                    "--no-watcher",
                ])
                .current_dir(&cwd)
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

        // Precondition: nothing captured yet for this scope.
        assert_eq!(
            session_count(&client, &base, WORKSPACE, PROJECT).await,
            0,
            "a brand-new project store must start with no captured sessions",
        );

        // Phase 1 — gap detected. `--since-days 0` makes every on-disk session
        // count as recent, so the planted transcript is unambiguously "recent".
        let report: Value = serde_json::from_str(&run_cli(
            &[
                "doctor",
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
                "--since-days",
                "0",
                "--json",
            ],
            data_dir.path(),
            home.path(),
            Some(&cwd),
            &base,
        ))
        .expect("doctor --json report");

        let claude = find_row(&report, "claude-code")
            .unwrap_or_else(|| panic!("expected a claude-code row: {report}"));
        assert_eq!(
            claude["uncaptured"], true,
            "a harness that ran here with zero captured must be flagged: {report}"
        );
        assert!(
            claude["local_recent"].as_u64().unwrap_or(0) >= 1,
            "the planted session must count as recent: {report}"
        );
        assert_eq!(
            claude["captured"], 0,
            "the server captured nothing for it yet: {report}"
        );
        assert_eq!(
            report["uncaptured"]
                .as_array()
                .map(|a| a.iter().any(|v| v == "claude-code")),
            Some(true),
            "claude-code must appear in the top-level uncaptured list: {report}"
        );

        // Phase 1 — the human-readable output prints the exact remediation the
        // operator is meant to run, naming the specific agent.
        let human = run_cli(
            &[
                "doctor",
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
                "--since-days",
                "0",
            ],
            data_dir.path(),
            home.path(),
            Some(&cwd),
            &base,
        );
        assert!(
            human.contains("ai-memory install-hooks --agent claude-code --apply"),
            "doctor must print the exact install-hooks remediation: {human}"
        );
        assert!(
            human.contains("ran here but nothing was captured"),
            "doctor must explain the gap in prose: {human}"
        );

        // Import the local history through the same `/hook` ingress live capture
        // uses, so the server now has captured sessions for claude-code.
        let backfill: Value = serde_json::from_str(&run_cli(
            &[
                "backfill",
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
                "--json",
            ],
            data_dir.path(),
            home.path(),
            Some(&cwd),
            &base,
        ))
        .expect("backfill --json report");
        assert_eq!(
            backfill["imported_sessions"], 1,
            "backfill must import the one planted session: {backfill}"
        );

        // The captured count is now nonzero (poll briefly for commit lag).
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if session_count(&client, &base, WORKSPACE, PROJECT).await >= 1 {
                break;
            }
            assert!(Instant::now() < deadline, "captured session never appeared");
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        // Phase 2 — healthy control. The same harness with the same local
        // transcript is now captured, so doctor must NOT flag it.
        let report2: Value = serde_json::from_str(&run_cli(
            &[
                "doctor",
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
                "--since-days",
                "0",
                "--json",
            ],
            data_dir.path(),
            home.path(),
            Some(&cwd),
            &base,
        ))
        .expect("second doctor --json report");

        let claude2 = find_row(&report2, "claude-code")
            .unwrap_or_else(|| panic!("expected a claude-code row after backfill: {report2}"));
        assert_eq!(
            claude2["uncaptured"], false,
            "one captured session must clear the flag: {report2}"
        );
        assert!(
            claude2["captured"].as_u64().unwrap_or(0) >= 1,
            "the backfilled session must be counted as captured: {report2}"
        );
        assert_eq!(
            report2["uncaptured"]
                .as_array()
                .map(|a| a.iter().any(|v| v == "claude-code")),
            Some(false),
            "claude-code must no longer be in the uncaptured list: {report2}"
        );

        // Phase 2 — the human output no longer nags for claude-code's hook.
        let human2 = run_cli(
            &[
                "doctor",
                "--workspace",
                WORKSPACE,
                "--project",
                PROJECT,
                "--since-days",
                "0",
            ],
            data_dir.path(),
            home.path(),
            Some(&cwd),
            &base,
        );
        assert!(
            !human2.contains("install-hooks --agent claude-code --apply"),
            "a captured harness must not get an install-hooks nag: {human2}"
        );

        drop(server);
    }
}
