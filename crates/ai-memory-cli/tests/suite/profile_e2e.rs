//! Use-case end-to-end test for the cross-project profile
//! (`docs/cross-project-profile.md`), driven the way a user meets it: they
//! state a habit in two projects, the profile learns it, and in a brand-new
//! repository `ai-memory profile apply` writes it into the rules file. Re-running
//! changes nothing, and `--remove` restores the file exactly.

/// Spawns a server and several subprocesses: seconds, not milliseconds, so this
/// lives in the slow tier (`cargo tf` / CI), not the everyday loop.
mod slow {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::e2e_support::{hermetic, run_cli, session_count, start_serve};

    const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");

    /// Post one user prompt for `project` through the real hook ingress and
    /// wait until the server has stored its session.
    async fn say(client: &reqwest::Client, base: &str, project: &str, nth: u8, prompt: &str) {
        let session_id = format!("00000000-0000-4000-8000-0000000001{nth:02x}");
        let cwd = format!("/tmp/profile-e2e/{project}");
        for (event, body) in [
            (
                "session-start",
                json!({ "session_id": session_id, "cwd": cwd }),
            ),
            (
                "user-prompt-submit",
                json!({ "session_id": session_id, "cwd": cwd, "prompt": prompt }),
            ),
        ] {
            let resp = client
                .post(format!("{base}/hook"))
                .query(&[("event", event), ("agent", "claude-code")])
                .header("content-type", "application/json")
                .body(body.to_string())
                .send()
                .await
                .expect("hook request");
            assert!(resp.status().is_success(), "{event}: {}", resp.status());
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while session_count(client, base, "default", project).await < 1 {
            assert!(Instant::now() < deadline, "{project} was never stored");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn apply(repo: &Path, data_dir: &Path, home: &Path, base: &str, extra: &[&str]) -> String {
        let mut args = vec!["profile", "apply"];
        args.extend_from_slice(extra);
        run_cli(&args, data_dir, home, Some(repo), base)
    }

    #[tokio::test]
    async fn show_and_forget_accept_optional_md_without_hiding_exact_paths() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let client = reqwest::Client::new();
        let (_server, base) = start_serve(&client, &data_dir.path().join("serve.log"), |port| {
            let mut cmd = hermetic(BIN);
            cmd.args([
                "serve",
                "--transport",
                "http",
                "--bind",
                &format!("127.0.0.1:{port}"),
                "--no-watcher",
            ])
            .env("AI_MEMORY_DATA_DIR", data_dir.path())
            .env("AI_MEMORY_HOME", home.path())
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none");
            cmd
        })
        .await;
        let run =
            |args: &[&str]| run_cli(args, data_dir.path(), home.path(), Some(home.path()), &base);
        for (path, body) in [
            ("profile/workflow/never-push.md", "Never push."),
            ("profile/workflow/exact", "The exact entry."),
            ("profile/workflow/exact.md", "The Markdown sibling."),
        ] {
            run(&[
                "write-page",
                "--workspace",
                "default",
                "--project",
                "_global",
                "--path",
                path,
                "--body",
                body,
            ]);
        }

        let shown = run(&["profile", "show", "workflow/never-push"]);
        assert!(shown.contains("profile/workflow/never-push.md"), "{shown}");
        assert!(shown.contains("Never push."), "{shown}");
        let forgotten = run(&["profile", "forget", "profile/workflow/never-push"]);
        assert!(
            forgotten.contains("Forgot profile/workflow/never-push.md"),
            "{forgotten}"
        );
        assert!(!run(&["profile", "list"]).contains("profile/workflow/never-push.md"));

        let shown = run(&["profile", "show", "workflow/exact"]);
        assert!(shown.contains("The exact entry."), "{shown}");
        let forgotten = run(&["profile", "forget", "workflow/exact"]);
        assert!(
            forgotten.contains("Forgot profile/workflow/exact ("),
            "{forgotten}"
        );
        let shown = run(&["profile", "show", "workflow/exact"]);
        assert!(shown.contains("The Markdown sibling."), "{shown}");

        for missing in ["workflow/not-an-entry", "profile/workflow/not-an-entry.md"] {
            let out = hermetic(BIN)
                .args(["profile", "forget", missing])
                .current_dir(home.path())
                .env("AI_MEMORY_DATA_DIR", data_dir.path())
                .env("AI_MEMORY_HOME", home.path())
                .env("AI_MEMORY_SERVER_URL", &base)
                .env("RUST_LOG", "off")
                .output()
                .expect("run profile forget");
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(!out.status.success(), "{stdout}");
            assert!(stderr.contains("nothing changed"), "{stderr}");
            assert!(!stdout.contains("Forgot"), "{stdout}");
        }
        let shown = run(&["profile", "show", "workflow/exact.md"]);
        assert!(shown.contains("The Markdown sibling."), "{shown}");
    }

    #[tokio::test]
    async fn a_habit_from_two_projects_is_applied_to_a_new_repository() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let client = reqwest::Client::new();
        let (server, base) = start_serve(&client, &data_dir.path().join("serve.log"), |port| {
            let mut cmd = hermetic(BIN);
            cmd.args([
                "serve",
                "--transport",
                "http",
                "--bind",
                &format!("127.0.0.1:{port}"),
                "--no-watcher",
            ])
            .env("AI_MEMORY_DATA_DIR", data_dir.path())
            .env("AI_MEMORY_HOME", home.path())
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none");
            cmd
        })
        .await;

        say(&client, &base, "alpha", 1, "I prefer tabs over spaces.").await;
        // Worded differently: the same sentence in two projects at once
        // reads as one fanned-out brief and counts once (#1148).
        say(
            &client,
            &base,
            "beta",
            2,
            "I really prefer tabs over spaces here too.",
        )
        .await;
        let rebuilt = run_cli(
            &["profile", "rebuild"],
            data_dir.path(),
            home.path(),
            None,
            &base,
        );
        assert!(rebuilt.contains("1 entry written"), "{rebuilt}");

        let workspace = tempfile::tempdir().expect("repos");
        let repo = workspace.path().join("gamma");
        std::fs::create_dir_all(repo.join(".git")).expect("fake checkout");
        let original = "# Gamma rules\n\nUse tabs? Decide later.\n";
        std::fs::write(repo.join("AGENTS.md"), original).expect("seed AGENTS.md");

        let dry = apply(&repo, data_dir.path(), home.path(), &base, &["--dry-run"]);
        assert!(dry.contains("tabs over spaces"), "{dry}");
        assert_eq!(
            std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
            original,
            "--dry-run writes nothing"
        );

        let out = apply(&repo, data_dir.path(), home.path(), &base, &[]);
        assert!(out.contains("updated"), "{out}");
        let written = std::fs::read_to_string(repo.join("AGENTS.md")).unwrap();
        assert!(written.starts_with(original), "{written}");
        assert!(written.contains("<!-- ai-memory:profile:start -->"));
        assert!(written.contains("tabs over spaces"), "{written}");

        let again = apply(&repo, data_dir.path(), home.path(), &base, &[]);
        assert!(again.contains("no-op"), "a re-run is idempotent: {again}");
        assert!(
            !repo.read_dir().unwrap().any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".bak")),
            "the backup stays out of the checkout"
        );

        apply(&repo, data_dir.path(), home.path(), &base, &["--remove"]);
        assert_eq!(
            std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
            original,
            "--remove restores the file"
        );
        drop(server);
    }

    /// A profile start line whose end was deleted by hand stays where it is,
    /// with the text after it: `apply` writes a fresh block after it, a re-run
    /// changes nothing, and `--remove` restores the file. Before, the second
    /// apply paired the stray start with the new block's end and replaced the
    /// user's text between them.
    #[tokio::test]
    async fn a_start_marker_that_lost_its_end_keeps_the_text_after_it() {
        let data_dir = tempfile::tempdir().expect("data dir");
        let home = tempfile::tempdir().expect("home");
        let client = reqwest::Client::new();
        let (server, base) = start_serve(&client, &data_dir.path().join("serve.log"), |port| {
            let mut cmd = hermetic(BIN);
            cmd.args([
                "serve",
                "--transport",
                "http",
                "--bind",
                &format!("127.0.0.1:{port}"),
                "--no-watcher",
            ])
            .env("AI_MEMORY_DATA_DIR", data_dir.path())
            .env("AI_MEMORY_HOME", home.path())
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none");
            cmd
        })
        .await;
        run_cli(
            &[
                "write-page",
                "--workspace",
                "default",
                "--project",
                "_global",
                "--path",
                "profile/tools/pnpm.md",
                "--body",
                "Use pnpm, not npm.",
            ],
            data_dir.path(),
            home.path(),
            None,
            &base,
        );

        let workspace = tempfile::tempdir().expect("repos");
        let repo = workspace.path().join("delta");
        std::fs::create_dir_all(repo.join(".git")).expect("fake checkout");
        let original = "# Delta rules\n<!-- ai-memory:profile:start -->\nmy notes\n";
        std::fs::write(repo.join("AGENTS.md"), original).expect("seed AGENTS.md");

        let out = apply(&repo, data_dir.path(), home.path(), &base, &[]);
        assert!(out.contains("updated"), "{out}");
        let first = std::fs::read_to_string(repo.join("AGENTS.md")).unwrap();
        assert!(first.starts_with(original), "{first}");
        assert!(first.contains("pnpm"), "{first}");

        let again = apply(&repo, data_dir.path(), home.path(), &base, &[]);
        assert!(again.contains("no-op"), "a re-run is idempotent: {again}");
        assert_eq!(
            std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
            first,
            "my notes survive the re-run"
        );

        apply(&repo, data_dir.path(), home.path(), &base, &["--remove"]);
        assert_eq!(
            std::fs::read_to_string(repo.join("AGENTS.md")).unwrap(),
            original,
            "--remove restores the file"
        );
        drop(server);
    }
}
