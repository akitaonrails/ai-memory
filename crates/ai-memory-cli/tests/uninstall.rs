//! End-to-end: install hooks into a temp HOME, then uninstall, and
//! assert the file round-trips (our entries gone, third-party intact).

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

#[test]
fn install_then_uninstall_round_trip_claude_hooks() {
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    // Pre-seed a third-party hook we must NOT touch.
    std::fs::write(
        claude.join("settings.json"),
        r#"{"hooks":{"Notification":[{"matcher":"","hooks":[{"type":"command","command":"/usr/bin/n.sh"}]}]}}"#,
    )
    .unwrap();

    // Install ai-memory hooks for Claude Code.
    let status = Command::new(bin())
        .args(["install-hooks", "--agent", "claude-code", "--apply"])
        .env("HOME", home.path())
        .status()
        .unwrap();
    assert!(status.success(), "install-hooks failed");

    // Uninstall (hooks only) and verify.
    let status = Command::new(bin())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .env("HOME", home.path())
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(claude.join("settings.json")).unwrap())
            .unwrap();
    // Third-party hook survived.
    assert!(after["hooks"]["Notification"].is_array());
    // None of our events remain.
    for ours in [
        "SessionStart",
        "SessionEnd",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "PreCompact",
        "UserPromptSubmit",
    ] {
        assert!(
            after["hooks"].get(ours).is_none(),
            "{ours} should be removed"
        );
    }
}

#[test]
fn uninstall_apply_is_idempotent() {
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("settings.json"),
        r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/stop.sh"}]}]}}"#,
    )
    .unwrap();

    let run = || {
        std::process::Command::new(bin())
            .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
            .env("HOME", home.path())
            .status()
            .unwrap()
    };

    assert!(run().success(), "first uninstall");
    // Count backups after first run.
    let count_baks = || {
        std::fs::read_dir(&claude)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".bak-"))
            .count()
    };
    let after_first = count_baks();
    assert!(run().success(), "second uninstall (idempotent)");
    assert_eq!(
        count_baks(),
        after_first,
        "second run must not create a new backup"
    );
}

#[test]
fn only_hooks_preserves_mcp_in_same_file() {
    // Gemini-style: hooks + mcpServers in one settings.json.
    let home = tempfile::tempdir().unwrap();
    let gem = home.path().join(".gemini");
    std::fs::create_dir_all(&gem).unwrap();
    std::fs::write(
        gem.join("settings.json"),
        r#"{"hooks":{"SessionStart":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/session-start.sh"}]}]},"mcpServers":{"ai-memory":{"httpUrl":"http://127.0.0.1:49374/mcp"}}}"#,
    )
    .unwrap();

    let status = std::process::Command::new(bin())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .env("HOME", home.path())
        .status()
        .unwrap();
    assert!(status.success());

    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(gem.join("settings.json")).unwrap()).unwrap();
    // Hooks removed...
    assert!(
        v["hooks"].get("SessionStart").is_none(),
        "hook should be removed"
    );
    // ...but the MCP entry must SURVIVE because --only hooks.
    assert!(
        v["mcpServers"].get("ai-memory").is_some(),
        "--only hooks must NOT touch mcpServers"
    );
}

#[test]
fn uninstall_dry_run_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let original = r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"}]}]}}"#;
    std::fs::write(claude.join("settings.json"), original).unwrap();

    let status = Command::new(bin())
        .args(["uninstall", "--only", "hooks"]) // no --apply
        .env("HOME", home.path())
        .status()
        .unwrap();
    assert!(status.success());

    let after = std::fs::read_to_string(claude.join("settings.json")).unwrap();
    assert_eq!(after, original, "dry-run must not modify the file");
}

#[test]
fn uninstall_purge_data_apply_wipes() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    for sub in ["wiki", "db", "raw"] {
        std::fs::create_dir_all(data.path().join(sub)).unwrap();
        std::fs::write(data.path().join(sub).join("f.txt"), b"x").unwrap();
    }
    std::fs::create_dir_all(data.path().join("logs")).unwrap();
    std::fs::write(data.path().join("logs/app.log"), b"l").unwrap();

    let out = Command::new(bin())
        .args(["uninstall", "--apply", "--yes", "--purge-data"])
        .env("HOME", home.path())
        .env("AI_MEMORY_DATA_DIR", data.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    for sub in ["wiki", "db", "raw"] {
        assert!(data.path().join(sub).is_dir(), "{sub} dir should remain");
        assert!(
            !data.path().join(sub).join("f.txt").exists(),
            "{sub} emptied"
        );
    }
    assert!(data.path().join("logs/app.log").exists(), "logs preserved");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("✓ purged"), "stdout was: {stdout}");
}

#[test]
fn uninstall_dry_run_previews_purge() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    for sub in ["wiki", "db", "raw"] {
        std::fs::create_dir_all(data.path().join(sub)).unwrap();
        std::fs::write(data.path().join(sub).join("f.txt"), b"x").unwrap();
    }

    let out = Command::new(bin())
        .args(["uninstall", "--purge-data"]) // dry-run: no --apply
        .env("HOME", home.path())
        .env("AI_MEMORY_DATA_DIR", data.path())
        .output()
        .unwrap();
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("would purge"), "stdout was: {stdout}");
    for sub in ["wiki", "db", "raw"] {
        let p = data.path().join(sub);
        assert!(
            stdout.contains(&p.display().to_string()),
            "missing {sub} in: {stdout}"
        );
        // Dry-run must not delete.
        assert!(p.join("f.txt").exists(), "{sub} must be untouched");
    }
}

/// Best-effort, NOT in the default run (sysinfo reads the real process table;
/// no injection seam). Spawns a real sibling `ai-memory` process and asserts
/// `--purge-data` refuses up front, leaving the wiring intact. Run with:
/// `cargo test -p ai-memory-cli --test uninstall -- --ignored`.
#[test]
#[ignore]
fn purge_data_refuses_when_sibling_alive() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let settings = claude.join("settings.json");
    let original = r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"}]}]}}"#;
    std::fs::write(&settings, original).unwrap();

    // Long-lived sibling `ai-memory` process.
    let mut serve = Command::new(bin())
        .arg("serve")
        .env("HOME", home.path())
        .env("AI_MEMORY_DATA_DIR", data.path())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));

    let out = Command::new(bin())
        .args(["uninstall", "--apply", "--yes", "--purge-data"])
        .env("HOME", home.path())
        .env("AI_MEMORY_DATA_DIR", data.path())
        .output()
        .unwrap();

    serve.kill().ok();
    serve.wait().ok();

    assert!(
        !out.status.success(),
        "should refuse while a sibling is alive"
    );
    // All-or-nothing: wiring must be untouched.
    assert_eq!(
        std::fs::read_to_string(&settings).unwrap(),
        original,
        "no wiring should be removed when the purge is refused up front"
    );
}
