//! End-to-end: add hooks into a temp HOME, then remove them, and
//! assert the file round-trips (our entries gone, third-party intact).

use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

use ai_memory_core::routing_skills::{MANAGED_MARKER, MANAGED_SKILLS};

static CLI_TEST_LOCK: Mutex<()> = Mutex::new(());

fn cli_test_lock() -> MutexGuard<'static, ()> {
    CLI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

fn command_with_home(home: &Path) -> Command {
    let mut command = Command::new(bin());
    let config_home = home.join(".config");
    let data_home = home.join(".local/share");
    let app_data = home.join("AppData/Roaming");
    let local_app_data = home.join("AppData/Local");
    for dir in [&config_home, &data_home, &app_data, &local_app_data] {
        std::fs::create_dir_all(dir).unwrap();
    }
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_DATA_HOME", data_home)
        .env("APPDATA", app_data)
        .env("LOCALAPPDATA", local_app_data)
        .env("AI_MEMORY_HOME", home)
        .env("AI_MEMORY_DATA_DIR", home.join(".ai-memory-data"));
    command
}

fn normalize_path_text(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .replace('\\', "/")
        .replace("//?/UNC/", "//")
        .replace("//?/", "")
}

fn run_uninstall(project: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    command_with_home(home)
        .args(args)
        .current_dir(project)
        .output()
        .unwrap()
}

fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn managed_skill_content() -> String {
    format!(
        "---\nname: test\n---\n{}\nmanaged test skill\n",
        MANAGED_MARKER
    )
}

#[test]
fn install_then_uninstall_round_trip_claude_hooks() {
    let _guard = cli_test_lock();
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
    let status = command_with_home(home.path())
        .args(["install-hooks", "--agent", "claude-code", "--apply"])
        .status()
        .unwrap();
    assert!(status.success(), "install-hooks failed");

    // Uninstall (hooks only) and verify.
    let status = command_with_home(home.path())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
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
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(
        claude.join("settings.json"),
        r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/stop.sh"}]}]}}"#,
    )
    .unwrap();

    let run = || {
        command_with_home(home.path())
            .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
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
    let _guard = cli_test_lock();
    // Gemini-style: hooks + mcpServers in one settings.json.
    let home = tempfile::tempdir().unwrap();
    let gem = home.path().join(".gemini");
    std::fs::create_dir_all(&gem).unwrap();
    std::fs::write(
        gem.join("settings.json"),
        r#"{"hooks":{"SessionStart":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/session-start.sh"}]}]},"mcpServers":{"ai-memory":{"httpUrl":"http://127.0.0.1:49374/mcp"}}}"#,
    )
    .unwrap();

    let status = command_with_home(home.path())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
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
fn uninstall_preserves_user_opencode_plugin_at_ai_memory_path() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join(".config/opencode/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let plugin = plugins.join("ai-memory.ts");
    let original = "// user-owned plugin that happens to use this filename\nexport default {};\n";
    std::fs::write(&plugin, original).unwrap();

    let status = command_with_home(home.path())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");

    assert_eq!(std::fs::read_to_string(&plugin).unwrap(), original);
}

#[test]
fn uninstall_deletes_generated_opencode_plugin_only() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let plugins = home.path().join(".config/opencode/plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let plugin = plugins.join("ai-memory.ts");
    std::fs::write(
        &plugin,
        "// Auto-generated by `ai-memory install-hooks --agent opencode --apply`.\nconst AGENT = \"open-code\";\n",
    )
    .unwrap();
    let sibling = plugins.join("other.ts");
    std::fs::write(&sibling, "keep me\n").unwrap();

    let status = command_with_home(home.path())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");

    assert!(!plugin.exists(), "generated plugin should be deleted");
    assert!(sibling.exists(), "unrelated plugin must be preserved");
}

#[test]
fn uninstall_omp_extension_deletes_only_generated_file() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let extensions = home.path().join(".omp/agent/extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    let extension = extensions.join("ai-memory.ts");
    let user_content = "// user-owned extension that happens to use this filename\n";
    std::fs::write(&extension, user_content).unwrap();

    let status = command_with_home(home.path())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");
    assert_eq!(std::fs::read_to_string(&extension).unwrap(), user_content);

    std::fs::write(
        &extension,
        "// Auto-generated by `ai-memory install-hooks --agent omp --apply`.\nconst AGENT = \"omp\";\n",
    )
    .unwrap();

    let status = command_with_home(home.path())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");
    assert!(!extension.exists(), "generated extension should be deleted");
}

#[test]
fn uninstall_pi_extension_deletes_only_generated_bridge_file() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let extensions = home.path().join(".pi/agent/extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    let extension = extensions.join("ai-memory.ts");
    let user_content = "// user-owned Pi extension\n";
    std::fs::write(&extension, user_content).unwrap();

    let status = command_with_home(home.path())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");
    assert_eq!(std::fs::read_to_string(&extension).unwrap(), user_content);

    std::fs::write(
        &extension,
        "// Auto-generated by `ai-memory install-hooks --agent pi --apply`.\nconst AGENT = \"pi\";\npi.registerTool({ name: \"memory_status\" });\n",
    )
    .unwrap();

    let status = command_with_home(home.path())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");
    assert!(
        !extension.exists(),
        "generated Pi extension should be deleted"
    );
}

#[test]
fn uninstall_preserves_user_openclaw_package_at_ai_memory_path() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join(".local/share");
    let plugin_dir = data.join("ai-memory/openclaw-plugin");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let package = plugin_dir.join("package.json");
    let original = r#"{"name":"@ai-memory/openclaw-plugin","private":true}"#;
    std::fs::write(&package, original).unwrap();

    let status = command_with_home(home.path())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .env("XDG_DATA_HOME", &data)
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");

    assert_eq!(std::fs::read_to_string(&package).unwrap(), original);
}

#[test]
fn uninstall_antigravity_hooks_preserves_user_entries() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".gemini/config");
    std::fs::create_dir_all(&config).unwrap();
    let hooks = config.join("hooks.json");
    std::fs::write(
        &hooks,
        r#"{
          "ai-memory": {
            "PreInvocation": [
              {"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/session-start.sh"},
              {"type":"command","command":"/usr/bin/user-pre-invocation"}
            ],
            "Stop": [
              {"type":"command","command":"AI_MEMORY_HOOK_URL=http://h /x/stop.sh"}
            ]
          },
          "other-group": {
            "Stop": [{"type":"command","command":"/usr/bin/other"}]
          }
        }"#,
    )
    .unwrap();

    let status = command_with_home(home.path())
        .args(["uninstall", "--apply", "--only", "hooks", "--yes"])
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
    assert_eq!(
        after["ai-memory"]["PreInvocation"]
            .as_array()
            .unwrap()
            .len(),
        1,
        "third-party entry in same group/event must survive"
    );
    assert!(after["ai-memory"].get("Stop").is_none());
    assert!(after.get("other-group").is_some());
}

#[test]
fn uninstall_mcp_custom_url_removes_antigravity_only_by_endpoint() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join(".gemini/antigravity-cli");
    std::fs::create_dir_all(&config).unwrap();
    let mcp = config.join("mcp_config.json");
    std::fs::write(
        &mcp,
        r#"{
          "mcpServers": {
            "ai-memory": {"serverUrl":"http://example.invalid/mcp"},
            "custom-memory": {"serverUrl":"http://lan:49374/mcp"},
            "other": {"serverUrl":"http://other/mcp"}
          }
        }"#,
    )
    .unwrap();

    let status = command_with_home(home.path())
        .args([
            "uninstall",
            "--apply",
            "--only",
            "mcp",
            "--mcp-url",
            "http://lan:49374/mcp",
            "--yes",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp).unwrap()).unwrap();
    assert!(after["mcpServers"].get("custom-memory").is_none());
    assert!(
        after["mcpServers"].get("ai-memory").is_some(),
        "same name with a different endpoint must survive"
    );
    assert!(after["mcpServers"].get("other").is_some());
}

#[test]
fn uninstall_mcp_name_narrows_endpoint_match() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude.json");
    std::fs::write(
        &claude,
        r#"{
          "mcpServers": {
            "ai-memory": {"url":"http://127.0.0.1:49374/mcp"},
            "ai-memory-alt": {"url":"http://127.0.0.1:49374/mcp"}
          }
        }"#,
    )
    .unwrap();

    let status = command_with_home(home.path())
        .args([
            "uninstall",
            "--apply",
            "--only",
            "mcp",
            "--mcp-name",
            "ai-memory",
            "--yes",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "uninstall failed");

    let after: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
    assert!(after["mcpServers"].get("ai-memory").is_none());
    assert!(after["mcpServers"].get("ai-memory-alt").is_some());
}

#[test]
fn uninstall_dry_run_changes_nothing() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let original = r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"}]}]}}"#;
    std::fs::write(claude.join("settings.json"), original).unwrap();

    let status = command_with_home(home.path())
        .args(["uninstall", "--only", "hooks"]) // no --apply
        .status()
        .unwrap();
    assert!(status.success());

    let after = std::fs::read_to_string(claude.join("settings.json")).unwrap();
    assert_eq!(after, original, "dry-run must not modify the file");
}

#[test]
fn default_uninstall_removes_managed_skills_across_roots_and_preserves_user_content() {
    let _guard = cli_test_lock();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let managed_content = managed_skill_content();

    let project_claude = project.path().join(".claude/skills");
    let project_agents = project.path().join(".agents/skills");
    let global_claude = home.path().join(".claude/skills");
    let global_agents = home.path().join(".agents/skills");

    let managed_paths = [
        project_claude.join(MANAGED_SKILLS[0].relative_path),
        project_agents.join(MANAGED_SKILLS[2].relative_path),
        global_claude.join(MANAGED_SKILLS[3].relative_path),
        global_agents.join(MANAGED_SKILLS[4].relative_path),
    ];
    for path in &managed_paths {
        write_file(path, &managed_content);
    }

    let unmanaged_same_name = project_claude.join(MANAGED_SKILLS[1].relative_path);
    let unmanaged_content = "---\nname: ai-memory-handoff\n---\nuser-owned same-name skill\n";
    write_file(&unmanaged_same_name, unmanaged_content);

    let unrelated_sibling = project_claude.join("user-skill/SKILL.md");
    write_file(&unrelated_sibling, "---\nname: user-skill\n---\nkeep me\n");

    let extra_file_in_managed_dir = managed_paths[1].parent().unwrap().join("notes.txt");
    write_file(&extra_file_in_managed_dir, "keep this sibling file\n");

    let output = run_uninstall(
        project.path(),
        home.path(),
        &["uninstall", "--apply", "--yes"],
    );
    assert!(
        output.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    for path in &managed_paths {
        assert!(
            !path.exists(),
            "managed skill file should be removed: {path:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(&unmanaged_same_name).unwrap(),
        unmanaged_content,
        "unmanaged same-name skill must be preserved"
    );
    assert!(
        unrelated_sibling.exists(),
        "unrelated sibling skill survives"
    );
    assert!(
        extra_file_in_managed_dir.exists(),
        "non-empty managed skill directory must not be removed"
    );
    assert!(
        !managed_paths[0].parent().unwrap().exists(),
        "empty managed skill directory should be removed"
    );
    assert!(
        !global_claude.exists() && !global_agents.exists(),
        "empty global skill roots should be removed"
    );
}

#[test]
fn install_skills_then_uninstall_only_skills_round_trips() {
    let _guard = cli_test_lock();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    let install = command_with_home(home.path())
        .args(["install-skills", "--scope", "project", "--agent", "both"])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "install-skills failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    for root in [
        project.path().join(".claude/skills"),
        project.path().join(".agents/skills"),
    ] {
        for skill in MANAGED_SKILLS {
            assert!(root.join(skill.relative_path).exists());
        }
    }

    let uninstall = run_uninstall(
        project.path(),
        home.path(),
        &["uninstall", "--only", "skills", "--apply", "--yes"],
    );
    assert!(
        uninstall.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&uninstall.stderr)
    );

    assert!(
        !project.path().join(".claude/skills").exists(),
        "empty Claude skills root should be removed"
    );
    assert!(
        !project.path().join(".agents/skills").exists(),
        "empty .agents skills root should be removed"
    );
}

#[test]
fn uninstall_only_skills_leaves_custom_target_dir_for_manual_cleanup() {
    let _guard = cli_test_lock();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let custom_root = project.path().join("custom-skills");

    let install = command_with_home(home.path())
        .args([
            "install-skills",
            "--target-dir",
            custom_root.to_str().unwrap(),
        ])
        .current_dir(project.path())
        .output()
        .unwrap();
    assert!(
        install.status.success(),
        "install-skills failed: {}",
        String::from_utf8_lossy(&install.stderr)
    );

    let custom_skill = custom_root.join(MANAGED_SKILLS[0].relative_path);
    assert!(custom_skill.exists());

    let uninstall = run_uninstall(
        project.path(),
        home.path(),
        &["uninstall", "--only", "skills", "--apply", "--yes"],
    );
    assert!(
        uninstall.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&uninstall.stderr)
    );

    assert!(
        custom_skill.exists(),
        "custom --target-dir skill roots are intentionally left for manual cleanup"
    );
    assert!(!project.path().join(".claude/skills").exists());
    assert!(!project.path().join(".agents/skills").exists());
}

#[test]
fn uninstall_skills_dry_run_reports_plan_without_mutating() {
    let _guard = cli_test_lock();
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let skill_path = project
        .path()
        .join(".claude/skills")
        .join(MANAGED_SKILLS[0].relative_path);
    let original = managed_skill_content();
    write_file(&skill_path, &original);

    let output = run_uninstall(
        project.path(),
        home.path(),
        &["uninstall", "--only", "skills"],
    );
    assert!(
        output.status.success(),
        "dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("would delete"), "stdout was: {stdout}");
    assert!(
        stdout.contains("managed Agent Skill"),
        "stdout was: {stdout}"
    );
    assert!(
        normalize_path_text(&stdout)
            .contains(&normalize_path_text(skill_path.display().to_string())),
        "stdout was: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(&skill_path).unwrap(),
        original,
        "dry-run must not remove or rewrite managed skill"
    );
}

#[test]
fn uninstall_purge_data_apply_wipes() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    for sub in ["wiki", "db", "raw"] {
        std::fs::create_dir_all(data.path().join(sub)).unwrap();
        std::fs::write(data.path().join(sub).join("f.txt"), b"x").unwrap();
    }
    std::fs::create_dir_all(data.path().join("logs")).unwrap();
    std::fs::write(data.path().join("logs/app.log"), b"l").unwrap();

    let out = command_with_home(home.path())
        .args(["uninstall", "--apply", "--yes", "--purge-data"])
        .env("AI_MEMORY_DATA_DIR", data.path())
        // Exercises the WIPE, not the live-process guard; opt out so an
        // unrelated `ai-memory` on the machine can't make it flake. The
        // dedicated guard test below does NOT set this.
        .env("AI_MEMORY_TEST_NO_PROCESS_GUARD", "1")
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
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    for sub in ["wiki", "db", "raw"] {
        std::fs::create_dir_all(data.path().join(sub)).unwrap();
        std::fs::write(data.path().join(sub).join("f.txt"), b"x").unwrap();
    }

    let out = command_with_home(home.path())
        .args(["uninstall", "--purge-data"]) // dry-run: no --apply
        .env("AI_MEMORY_DATA_DIR", data.path())
        // Dry-run still hits the purge guard before previewing; opt out so an
        // unrelated live `ai-memory` can't flake the preview.
        .env("AI_MEMORY_TEST_NO_PROCESS_GUARD", "1")
        .output()
        .unwrap();
    assert!(out.status.success());

    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("would purge"), "stdout was: {stdout}");
    let normalized_stdout = normalize_path_text(&stdout);
    for sub in ["wiki", "db", "raw"] {
        let p = data.path().join(sub);
        let expected_suffix = format!("/{sub}");
        assert!(
            normalized_stdout
                .lines()
                .any(|line| line.starts_with("would purge ") && line.ends_with(&expected_suffix)),
            "missing {sub} in: {stdout}"
        );
        // Dry-run must not delete.
        assert!(p.join("f.txt").exists(), "{sub} must be untouched");
    }
}

/// Best-effort, NOT in the default run (sysinfo reads the real process table;
/// no injection seam). Spawns a real sibling `ai-memory` process and asserts
/// `--purge-data` refuses up front, leaving the wiring intact. Run with:
/// `cargo test -p ai-memory-cli --test removal -- --ignored`.
#[test]
#[ignore]
fn purge_data_refuses_when_sibling_alive() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let claude = home.path().join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let settings = claude.join("settings.json");
    let original = r#"{"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"AI_MEMORY_HOOK_URL=x /a/stop.sh"}]}]}}"#;
    std::fs::write(&settings, original).unwrap();

    // Long-lived sibling `ai-memory` process.
    let mut serve = command_with_home(home.path())
        .arg("serve")
        .env("AI_MEMORY_DATA_DIR", data.path())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));

    let out = command_with_home(home.path())
        .args(["uninstall", "--apply", "--yes", "--purge-data"])
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
