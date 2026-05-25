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
