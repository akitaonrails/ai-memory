//! Integration tests for `ai-memory backup-agents` and `ai-memory restore-agents`.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, MutexGuard};

static CLI_TEST_LOCK: Mutex<()> = Mutex::new(());

fn cli_test_lock() -> MutexGuard<'static, ()> {
    CLI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

fn command_with_env(home: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(bin());
    let config_home = home.join(".config");
    let data_home = home.join(".local/share");
    for dir in [&config_home, &data_home] {
        fs::create_dir_all(dir).unwrap();
    }
    command
        .current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_DATA_HOME", data_home)
        .env("AI_MEMORY_HOME", home)
        .env("AI_MEMORY_DATA_DIR", home.join(".ai-memory-data"))
        .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
        .env_remove("AI_MEMORY_SERVER_URL")
        .env_remove("AI_MEMORY_AUTH_TOKEN")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("CODEX_HOME")
        .env_remove("COPILOT_HOME")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("KIMI_CODE_HOME")
        .env_remove("KIRO_HOME");
    #[cfg(windows)]
    command
        .env("APPDATA", home.join("AppData/Roaming"))
        .env("LOCALAPPDATA", home.join("AppData/Local"));
    command
}

fn claude_desktop_config_in(home: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(home.join("Library/Application Support/Claude/claude_desktop_config.json"))
    }
    #[cfg(target_os = "windows")]
    {
        Some(home.join("AppData/Roaming/Claude/claude_desktop_config.json"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = home;
        None
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

fn write_agent_archive(path: &Path, entries: serde_json::Value, bodies: &[(&str, &[u8])]) {
    let file = fs::File::create(path).unwrap();
    let encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let manifest = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "created_at": "2026-09-28T00:00:00Z",
        "host": { "os": "linux", "arch": "x86_64" },
        "sanitized": false,
        "entries": entries,
    }))
    .unwrap();
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    archive
        .append_data(&mut header, "manifest.json", manifest.as_slice())
        .unwrap();
    for (name, body) in bodies {
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        archive.append_data(&mut header, name, *body).unwrap();
    }
    archive.into_inner().unwrap().finish().unwrap();
}

#[test]
fn backup_and_restore_agents_round_trip() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    // 1. Setup mock agent assets
    // Claude
    let claude_settings = home.path().join(".claude/settings.json");
    fs::create_dir_all(claude_settings.parent().unwrap()).unwrap();
    fs::write(
        &claude_settings,
        r#"{"mcpServers":{"local":{"command":"test"}}}"#,
    )
    .unwrap();

    let claude_skill = home.path().join(".claude/skills/test-skill/SKILL.md");
    fs::create_dir_all(claude_skill.parent().unwrap()).unwrap();
    fs::write(&claude_skill, "# Claude Skill\nDetails here").unwrap();

    let claude_md = project.path().join("CLAUDE.md");
    fs::write(&claude_md, "# Project Instructions\nClaude rules").unwrap();

    // Codex
    let codex_config = home.path().join(".codex/config.toml");
    fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
    fs::write(
        &codex_config,
        "[mcp_servers.demo]\nurl = \"http://localhost\"\n",
    )
    .unwrap();

    let codex_hooks = home.path().join(".codex/hooks.json");
    let hooks =
        r#"{"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo ready"}]}]}}"#;
    fs::write(codex_hooks, hooks).unwrap();

    let agents_md = project.path().join("AGENTS.md");
    fs::write(&agents_md, "# Project Rules\nCodex rules").unwrap();
    let shared_skill = project.path().join(".agents/skills/shared/SKILL.md");
    fs::create_dir_all(shared_skill.parent().unwrap()).unwrap();
    fs::write(&shared_skill, "# Shared skill").unwrap();

    let opencode_config = home.path().join(".config/opencode/opencode.json");
    fs::create_dir_all(opencode_config.parent().unwrap()).unwrap();
    fs::write(&opencode_config, r#"{"mcp":{}}"#).unwrap();

    let desktop_config = claude_desktop_config_in(home.path());
    if let Some(path) = &desktop_config {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, r#"{"mcpServers":{"desktop":{}}}"#).unwrap();
    }

    // Antigravity / Gemini
    let agy_mcp = home.path().join(".gemini/config/mcp_config.json");
    fs::create_dir_all(agy_mcp.parent().unwrap()).unwrap();
    fs::write(
        &agy_mcp,
        r#"{"mcpServers":{"agy":{"url":"http://127.0.0.1"}}}"#,
    )
    .unwrap();

    let backup_tar = project.path().join("backup.tar.gz");
    fs::write(&backup_tar, "previous archive").unwrap();

    // 2. Run backup-agents
    let output = command_with_env(home.path(), project.path())
        .args([
            "backup-agents",
            "-o",
            backup_tar.to_str().unwrap(),
            "--include-secrets",
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "backup-agents failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&backup_tar).unwrap().permissions().mode() & 0o777,
            0o600,
            "archives containing secrets must be private before writing"
        );
    }
    assert!(backup_tar.exists(), "backup tarball was not created");

    // 3. Restore into clean sandbox
    let restore_home = tempfile::tempdir().unwrap();
    let restore_project = tempfile::tempdir().unwrap();

    // Dry-run first
    let dry_run = command_with_env(restore_home.path(), restore_project.path())
        .args(["restore-agents", "-i", backup_tar.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(dry_run.status.success());
    let dry_stdout = String::from_utf8_lossy(&dry_run.stdout);
    assert!(dry_stdout.contains("[CREATE]"));
    assert!(!restore_home.path().join(".claude/settings.json").exists());

    // Apply restore
    let restore = command_with_env(restore_home.path(), restore_project.path())
        .args([
            "restore-agents",
            "-i",
            backup_tar.to_str().unwrap(),
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "restore-agents failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );

    // Verify restored assets
    let restored_claude =
        fs::read_to_string(restore_home.path().join(".claude/settings.json")).unwrap();
    assert_eq!(
        restored_claude,
        r#"{"mcpServers":{"local":{"command":"test"}}}"#
    );

    let restored_skill = fs::read_to_string(
        restore_home
            .path()
            .join(".claude/skills/test-skill/SKILL.md"),
    )
    .unwrap();
    assert_eq!(restored_skill, "# Claude Skill\nDetails here");

    let restored_claude_md = fs::read_to_string(restore_project.path().join("CLAUDE.md")).unwrap();
    assert_eq!(restored_claude_md, "# Project Instructions\nClaude rules");

    let restored_codex =
        fs::read_to_string(restore_home.path().join(".codex/config.toml")).unwrap();
    assert_eq!(
        restored_codex,
        "[mcp_servers.demo]\nurl = \"http://localhost\"\n"
    );
    assert_eq!(
        fs::read_to_string(restore_home.path().join(".codex/hooks.json")).unwrap(),
        hooks
    );

    let restored_agents_md = fs::read_to_string(restore_project.path().join("AGENTS.md")).unwrap();
    assert_eq!(restored_agents_md, "# Project Rules\nCodex rules");
    assert_eq!(
        fs::read_to_string(
            restore_project
                .path()
                .join(".agents/skills/shared/SKILL.md")
        )
        .unwrap(),
        "# Shared skill"
    );

    let restored_agy =
        fs::read_to_string(restore_home.path().join(".gemini/config/mcp_config.json")).unwrap();
    assert_eq!(
        restored_agy,
        r#"{"mcpServers":{"agy":{"url":"http://127.0.0.1"}}}"#
    );
    assert!(
        restore_home
            .path()
            .join(".config/opencode/opencode.json")
            .exists()
    );
    if desktop_config.is_some() {
        assert!(
            claude_desktop_config_in(restore_home.path())
                .unwrap()
                .exists()
        );
    }
}

#[test]
fn antigravity_backup_round_trips_every_documented_skill_root() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let fixtures = [
        (
            home.path().join(".gemini/config/skills/app/SKILL.md"),
            "# App and IDE skill",
        ),
        (
            home.path()
                .join(".gemini/antigravity-cli/skills/cli/SKILL.md"),
            "# CLI skill",
        ),
        (
            project.path().join(".agents/skills/workspace/SKILL.md"),
            "# Workspace skill",
        ),
    ];
    for (path, body) in &fixtures {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let obsolete = project.path().join(".gemini/skills/obsolete/SKILL.md");
    fs::create_dir_all(obsolete.parent().unwrap()).unwrap();
    fs::write(&obsolete, "# Old undocumented root").unwrap();

    let archive = project.path().join("antigravity.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .args([
            "backup-agents",
            "-o",
            archive.to_str().unwrap(),
            "--agents",
            "antigravity",
            "--include-secrets",
        ])
        .output()
        .unwrap();
    assert!(
        backup.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&backup.stderr)
    );

    let restored_home = tempfile::tempdir().unwrap();
    let restored_project = tempfile::tempdir().unwrap();
    let restore = command_with_env(restored_home.path(), restored_project.path())
        .args([
            "restore-agents",
            "-i",
            archive.to_str().unwrap(),
            "--agents",
            "antigravity",
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_eq!(
        fs::read_to_string(
            restored_home
                .path()
                .join(".gemini/config/skills/app/SKILL.md")
        )
        .unwrap(),
        "# App and IDE skill"
    );
    assert_eq!(
        fs::read_to_string(
            restored_home
                .path()
                .join(".gemini/antigravity-cli/skills/cli/SKILL.md")
        )
        .unwrap(),
        "# CLI skill"
    );
    assert_eq!(
        fs::read_to_string(
            restored_project
                .path()
                .join(".agents/skills/workspace/SKILL.md")
        )
        .unwrap(),
        "# Workspace skill"
    );
    assert!(
        !restored_project
            .path()
            .join(".gemini/skills/obsolete/SKILL.md")
            .exists(),
        "the undocumented legacy project path must not be archived"
    );
}

fn backup_agents_args(archive: &Path, agent: &str) -> Vec<String> {
    vec![
        "backup-agents".to_string(),
        "-o".to_string(),
        archive.display().to_string(),
        "--agents".to_string(),
        agent.to_string(),
    ]
}

fn restore_agents_args(archive: &Path, agent: &str) -> Vec<String> {
    vec![
        "restore-agents".to_string(),
        "-i".to_string(),
        archive.display().to_string(),
        "--agents".to_string(),
        agent.to_string(),
        "--apply".to_string(),
    ]
}

fn assert_success(output: &std::process::Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn codex_assets_follow_codex_home_on_backup_and_restore() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let codex_home = home.path().join("active Codex home");
    let fixtures = [
        (
            "config.toml",
            "[mcp_servers.demo]\nurl = \"http://localhost\"\n",
        ),
        ("mcp.json", r#"{"mcpServers":{}}"#),
        ("skills/personal/SKILL.md", "# Personal Codex skill"),
        (
            "plugins/local/.codex-plugin/plugin.json",
            r#"{"name":"local"}"#,
        ),
    ];
    for (relative, body) in fixtures {
        let path = codex_home.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    let fake_token = "ghp_FAKE0123456789ABCDEFGHIJKLMNOPQRSTUV";
    let hooks = serde_json::json!({"hooks":{"SessionStart":[{"hooks":[{
        "type":"command",
        "command": format!("echo {fake_token}")
    }]}]}});
    fs::write(codex_home.join("hooks.json"), hooks.to_string()).unwrap();
    let shared_skill = home.path().join(".agents/skills/shared/SKILL.md");
    fs::create_dir_all(shared_skill.parent().unwrap()).unwrap();
    fs::write(shared_skill, "# Shared skill").unwrap();
    let inactive_skill = home.path().join(".codex/skills/stale/SKILL.md");
    fs::create_dir_all(inactive_skill.parent().unwrap()).unwrap();
    fs::write(inactive_skill, "# Stale skill").unwrap();

    let archive = project.path().join("codex-home.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .env("CODEX_HOME", &codex_home)
        .args(backup_agents_args(&archive, "codex"))
        .output()
        .unwrap();
    assert_success(&backup, "backup");

    let restored_home = tempfile::tempdir().unwrap();
    let destination = restored_home.path().join("destination Codex home");
    let restore = command_with_env(restored_home.path(), project.path())
        .env("CODEX_HOME", &destination)
        .args(restore_agents_args(&archive, "codex"))
        .output()
        .unwrap();
    assert_success(&restore, "restore");

    for (relative, body) in fixtures {
        assert_eq!(
            fs::read_to_string(destination.join(relative)).unwrap(),
            body
        );
    }
    let restored_hooks = fs::read_to_string(destination.join("hooks.json")).unwrap();
    assert!(!restored_hooks.contains(fake_token));
    assert!(restored_hooks.contains("[REDACTED:"));
    assert!(serde_json::from_str::<serde_json::Value>(&restored_hooks).is_ok());
    assert_eq!(
        fs::read_to_string(restored_home.path().join(".agents/skills/shared/SKILL.md")).unwrap(),
        "# Shared skill"
    );
    assert!(!restored_home.path().join(".codex").exists());
    assert!(!destination.join("skills/stale").exists());
}

#[test]
fn gemini_assets_follow_gemini_cli_home_on_backup_and_restore() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let gemini_home = home.path().join("active Gemini home");
    let settings = ".gemini/settings.json";
    let skill = ".gemini/skills/example/SKILL.md";
    let active_settings = r#"{"ui":{"theme":"Active profile"}}"#;
    let stale_settings = r#"{"ui":{"theme":"Default profile"}}"#;
    for (path, body) in [
        (gemini_home.join(settings), active_settings),
        (gemini_home.join(skill), "# Example Gemini skill"),
        (home.path().join(settings), stale_settings),
    ] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    let archive = project.path().join("gemini-home.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .env("GEMINI_CLI_HOME", &gemini_home)
        .args(backup_agents_args(&archive, "gemini"))
        .output()
        .unwrap();
    assert_success(&backup, "backup");

    let restored_home = tempfile::tempdir().unwrap();
    let destination = restored_home.path().join("destination Gemini home");
    let restore = command_with_env(restored_home.path(), project.path())
        .env("GEMINI_CLI_HOME", &destination)
        .args(restore_agents_args(&archive, "gemini"))
        .output()
        .unwrap();
    assert_success(&restore, "restore");
    assert_eq!(
        fs::read_to_string(destination.join(settings)).unwrap(),
        active_settings
    );
    assert_eq!(
        fs::read_to_string(destination.join(skill)).unwrap(),
        "# Example Gemini skill"
    );
    assert!(!restored_home.path().join(".gemini").exists());
}

/// Copilot CLI reads personal skills from `$COPILOT_HOME/skills` (default
/// `~/.copilot/skills`) and repository skills from `.github/skills`, the
/// roots `install-skills --agent copilot-cli` writes:
/// <https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-skills>.
/// The `~/.agents/skills`, `.agents/skills` and `.claude/skills` roots the
/// same page lists are already collected for Codex and Claude Code.
#[test]
fn copilot_cli_backup_round_trips_personal_and_repository_skills() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let fixtures = [
        (
            home.path().join(".copilot/skills/personal/SKILL.md"),
            "# Personal Copilot skill",
        ),
        (
            home.path().join(".copilot/skills/personal/scripts/run.sh"),
            "#!/bin/sh\necho personal",
        ),
        (
            project.path().join(".github/skills/repo/SKILL.md"),
            "# Repository Copilot skill",
        ),
    ];
    for (path, body) in &fixtures {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let not_a_skill = home.path().join(".copilot/mcp-config.json");
    fs::write(&not_a_skill, r#"{"mcpServers":{}}"#).unwrap();

    let archive = project.path().join("copilot.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .args(backup_agents_args(&archive, "copilot-cli"))
        .output()
        .unwrap();
    assert_success(&backup, "backup");

    let restored_home = tempfile::tempdir().unwrap();
    let restored_project = tempfile::tempdir().unwrap();
    let restore = command_with_env(restored_home.path(), restored_project.path())
        .args(restore_agents_args(&archive, "copilot-cli"))
        .output()
        .unwrap();
    assert_success(&restore, "restore");

    let restored = [
        (
            restored_home
                .path()
                .join(".copilot/skills/personal/SKILL.md"),
            "# Personal Copilot skill",
        ),
        (
            restored_home
                .path()
                .join(".copilot/skills/personal/scripts/run.sh"),
            "#!/bin/sh\necho personal",
        ),
        (
            restored_project.path().join(".github/skills/repo/SKILL.md"),
            "# Repository Copilot skill",
        ),
    ];
    for (path, body) in &restored {
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            *body,
            "{}",
            path.display()
        );
    }
    assert!(
        !restored_home
            .path()
            .join(".copilot/mcp-config.json")
            .exists(),
        "only Copilot CLI skills are collected, not its other config files"
    );
}

#[test]
fn copilot_cli_skills_follow_copilot_home_on_backup_and_restore() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let copilot_home = home.path().join("custom-copilot");
    let skill = copilot_home.join("skills/moved/SKILL.md");
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(&skill, "# Moved Copilot skill").unwrap();
    let default_root_skill = home.path().join(".copilot/skills/ignored/SKILL.md");
    fs::create_dir_all(default_root_skill.parent().unwrap()).unwrap();
    fs::write(&default_root_skill, "# Not the active root").unwrap();

    let archive = project.path().join("copilot-home.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .env("COPILOT_HOME", &copilot_home)
        .args(backup_agents_args(&archive, "copilot-cli"))
        .output()
        .unwrap();
    assert_success(&backup, "backup");

    let restored_home = tempfile::tempdir().unwrap();
    let restored_copilot_home = restored_home.path().join("elsewhere");
    let restore = command_with_env(restored_home.path(), project.path())
        .env("COPILOT_HOME", &restored_copilot_home)
        .args(restore_agents_args(&archive, "copilot-cli"))
        .output()
        .unwrap();
    assert_success(&restore, "restore");

    assert_eq!(
        fs::read_to_string(restored_copilot_home.join("skills/moved/SKILL.md")).unwrap(),
        "# Moved Copilot skill"
    );
    assert!(
        !restored_home.path().join(".copilot").exists(),
        "with COPILOT_HOME set, nothing lands in the default ~/.copilot"
    );
    assert!(
        !restored_copilot_home.join("skills/ignored").exists(),
        "the default root is not collected while COPILOT_HOME points elsewhere"
    );
}

#[test]
fn opencode_global_skills_round_trip_through_backup_and_restore() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let config = r#"{"$schema":"https://opencode.ai/config.json"}"#;
    let skill = "# Example OpenCode skill";
    for (path, body) in [
        (home.path().join(".config/opencode/opencode.json"), config),
        (
            home.path().join(".config/opencode/skills/example/SKILL.md"),
            skill,
        ),
    ] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    let archive = project.path().join("opencode.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .args(backup_agents_args(&archive, "opencode"))
        .output()
        .unwrap();
    assert_success(&backup, "backup");

    let restored_home = tempfile::tempdir().unwrap();
    let restore = command_with_env(restored_home.path(), project.path())
        .args(restore_agents_args(&archive, "opencode"))
        .output()
        .unwrap();
    assert_success(&restore, "restore");

    let restored_root = restored_home.path().join(".config/opencode");
    assert_eq!(
        fs::read_to_string(restored_root.join("skills/example/SKILL.md")).unwrap(),
        skill
    );
    assert_eq!(
        fs::read_to_string(restored_root.join("opencode.json")).unwrap(),
        config
    );
}

/// The Copilot allowlist entries cover skills only: an archive cannot use
/// them to reach Copilot's MCP or hook configuration, or other `.github`
/// files such as workflows. The skill paths stay restorable as a control.
#[test]
fn restore_agents_limits_copilot_destinations_to_skill_roots() {
    let _guard = cli_test_lock();
    let refused = [
        ("global", ".copilot/mcp-config.json"),
        ("global", ".copilot/hooks/ai-memory.json"),
        ("project", ".github/workflows/ci.yml"),
        ("project", ".github/hooks/ai-memory.json"),
    ];
    for (scope, target) in refused {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let archive = project.path().join("copilot-evil.tar.gz");
        let body: &[u8] = b"payload";
        write_agent_archive(
            &archive,
            serde_json::json!([{
                "agent": "copilot-cli",
                "asset_kind": "skill",
                "scope": scope,
                "archive_path": "payload",
                "target_relative": target,
                "sha256": sha256(body),
            }]),
            &[("payload", body)],
        );
        let restore = command_with_env(home.path(), project.path())
            .args(restore_agents_args(&archive, "copilot-cli"))
            .output()
            .unwrap();
        assert!(
            !restore.status.success(),
            "{scope} {target} must be refused"
        );
        assert!(
            String::from_utf8_lossy(&restore.stderr).contains("unauthorized path"),
            "{scope} {target}: {}",
            String::from_utf8_lossy(&restore.stderr)
        );
        assert!(!home.path().join(target).exists());
        assert!(!project.path().join(target).exists());
    }

    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let archive = project.path().join("copilot-ok.tar.gz");
    let body: &[u8] = b"# ok";
    write_agent_archive(
        &archive,
        serde_json::json!([
            {
                "agent": "copilot-cli",
                "asset_kind": "skill",
                "scope": "global",
                "archive_path": "global",
                "target_relative": ".copilot/skills/ok/SKILL.md",
                "sha256": sha256(body),
            },
            {
                "agent": "copilot-cli",
                "asset_kind": "skill",
                "scope": "project",
                "archive_path": "project",
                "target_relative": ".github/skills/ok/SKILL.md",
                "sha256": sha256(body),
            }
        ]),
        &[("global", body), ("project", body)],
    );
    let restore = command_with_env(home.path(), project.path())
        .args(restore_agents_args(&archive, "copilot-cli"))
        .output()
        .unwrap();
    assert_success(&restore, "restore of skill paths");
    assert_eq!(
        fs::read(home.path().join(".copilot/skills/ok/SKILL.md")).unwrap(),
        body
    );
    assert_eq!(
        fs::read(project.path().join(".github/skills/ok/SKILL.md")).unwrap(),
        body
    );
}

#[test]
fn kimi_code_backup_round_trips_mcp_hooks_and_skills() {
    let _guard = cli_test_lock();
    for (source_dir, restore_dir) in [
        (".kimi-code", ".kimi-code"),
        ("custom-kimi", "relocated-kimi"),
    ] {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let kimi_home = home.path().join(source_dir);
        let command = || {
            let mut command = command_with_env(home.path(), project.path());
            if source_dir != ".kimi-code" {
                command.env("KIMI_CODE_HOME", &kimi_home);
            }
            command
        };
        let token = "FAKEfake0123456789";
        let mcp = command()
            .args([
                "install-mcp",
                "--client",
                "kimi-code",
                "--apply",
                "--server-url",
                "http://127.0.0.1:49374/mcp",
                "--auth-token",
                token,
            ])
            .output()
            .unwrap();
        assert_success(&mcp, "install MCP config");
        assert!(
            fs::read_to_string(kimi_home.join("mcp.json"))
                .unwrap()
                .contains(token)
        );
        let hook_config = kimi_home.join("config.toml");
        fs::write(
            &hook_config,
            format!(
                "default_model = \"fixture-model\"\n\n[providers.fixture]\napi_key = \"{token}\"\n"
            ),
        )
        .unwrap();
        let hooks_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hooks");
        let hooks = command()
            .args([
                "install-hooks",
                "--agent",
                "kimi-code",
                "--apply",
                "--hooks-dir",
            ])
            .arg(hooks_dir)
            .args(["--server-url", "http://127.0.0.1:49374"])
            .output()
            .unwrap();
        assert_success(&hooks, "install hook config");
        let installed_toml = fs::read_to_string(hook_config)
            .unwrap()
            .parse::<toml_edit::DocumentMut>()
            .unwrap();
        let installed_hooks = installed_toml["hooks"].as_array_of_tables().unwrap();
        assert!(!installed_hooks.is_empty());
        let credentials = kimi_home.join("credentials");
        fs::create_dir_all(&credentials).unwrap();
        fs::write(credentials.join("fixture.json"), "{}").unwrap();
        let skill = kimi_home.join("skills/current/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(&skill, "# Current Kimi skill").unwrap();
        let legacy_mcp = ".kimi/mcp.json";
        fs::create_dir_all(home.path().join(".kimi")).unwrap();
        fs::write(home.path().join(legacy_mcp), r#"{"mcpServers":{}}"#).unwrap();

        let archive = project.path().join("kimi-code.tar.gz");
        let backup = command()
            .args(backup_agents_args(&archive, "kimi-code"))
            .args(["--scope", "global"])
            .output()
            .unwrap();
        assert_success(&backup, "backup");

        let restored_home = tempfile::tempdir().unwrap();
        let restored_kimi_home = restored_home.path().join(restore_dir);
        let mut restore = command_with_env(restored_home.path(), project.path());
        if restore_dir != ".kimi-code" {
            restore.env("KIMI_CODE_HOME", &restored_kimi_home);
        }
        let restored = restore
            .args(restore_agents_args(&archive, "kimi-code"))
            .output()
            .unwrap();
        assert_success(&restored, "restore");
        let restored_mcp = fs::read_to_string(restored_kimi_home.join("mcp.json")).unwrap();
        assert!(!restored_mcp.contains(token));
        let mcp: serde_json::Value = serde_json::from_str(&restored_mcp).unwrap();
        assert_eq!(
            mcp["mcpServers"]["ai-memory"]["url"],
            "http://127.0.0.1:49374/mcp?flavor=moonshot"
        );
        let restored_config = fs::read_to_string(restored_kimi_home.join("config.toml")).unwrap();
        assert!(!restored_config.contains(token));
        let restored_toml = restored_config.parse::<toml_edit::DocumentMut>().unwrap();
        let restored_hooks = restored_toml["hooks"].as_array_of_tables().unwrap();
        assert_eq!(restored_hooks.to_string(), installed_hooks.to_string());
        assert_eq!(
            restored_toml["default_model"].as_str(),
            Some("fixture-model")
        );
        assert!(
            restored_toml["providers"]["fixture"]["api_key"]
                .as_str()
                .is_some_and(|value| value.starts_with("[REDACTED"))
        );
        assert!(!restored_kimi_home.join("credentials").exists());
        assert_eq!(
            fs::read_to_string(restored_kimi_home.join("skills/current/SKILL.md")).unwrap(),
            "# Current Kimi skill"
        );
        assert_eq!(
            fs::read_to_string(restored_home.path().join(legacy_mcp)).unwrap(),
            r#"{"mcpServers":{}}"#
        );
        if restore_dir != ".kimi-code" {
            assert!(!restored_home.path().join(".kimi-code").exists());
        }
    }
}

#[test]
fn qwen_backup_round_trips_every_documented_asset() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let fixtures = [
        (
            home.path().join(".qwen/settings.json"),
            r#"{"mcpServers":{"ai-memory":{"httpUrl":"http://localhost"}}}"#,
        ),
        (
            home.path().join(".qwen/skills/qwen/SKILL.md"),
            "# Qwen skill",
        ),
        (
            home.path().join(".agents/skills/shared/SKILL.md"),
            "# Shared skill",
        ),
        (project.path().join("QWEN.md"), "# Qwen instructions"),
        (project.path().join("AGENTS.md"), "# Shared instructions"),
    ];
    for (path, body) in &fixtures {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    let archive = project.path().join("qwen.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .args([
            "backup-agents",
            "-o",
            archive.to_str().unwrap(),
            "--agents",
            "qwen-code",
            "--include-secrets",
        ])
        .output()
        .unwrap();
    assert!(
        backup.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&backup.stderr)
    );

    let restored_home = tempfile::tempdir().unwrap();
    let restored_project = tempfile::tempdir().unwrap();
    let restore = command_with_env(restored_home.path(), restored_project.path())
        .args([
            "restore-agents",
            "-i",
            archive.to_str().unwrap(),
            "--agents",
            "qwen-code",
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );

    for (path, body) in [
        (
            restored_home.path().join(".qwen/settings.json"),
            fixtures[0].1,
        ),
        (
            restored_home.path().join(".qwen/skills/qwen/SKILL.md"),
            fixtures[1].1,
        ),
        (
            restored_home.path().join(".agents/skills/shared/SKILL.md"),
            fixtures[2].1,
        ),
        (restored_project.path().join("QWEN.md"), fixtures[3].1),
        (restored_project.path().join("AGENTS.md"), fixtures[4].1),
    ] {
        assert_eq!(fs::read_to_string(path).unwrap(), body);
    }
}

#[test]
fn qoder_backup_round_trips_shared_assets() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let fixtures = [
        (
            home.path().join(".qoder/settings.json"),
            r#"{"mcpServers":{"ai-memory":{"command":"ai-memory"}}}"#,
        ),
        (
            home.path().join(".agents/skills/shared/SKILL.md"),
            "# Shared skill",
        ),
        (project.path().join("AGENTS.md"), "# Shared instructions"),
    ];
    for (path, body) in &fixtures {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    let archive = project.path().join("qoder.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .args([
            "backup-agents",
            "-o",
            archive.to_str().unwrap(),
            "--agents",
            "qoder",
            "--include-secrets",
        ])
        .output()
        .unwrap();
    assert!(
        backup.status.success(),
        "backup failed: {}",
        String::from_utf8_lossy(&backup.stderr)
    );

    let restored_home = tempfile::tempdir().unwrap();
    let restored_project = tempfile::tempdir().unwrap();
    let restore = command_with_env(restored_home.path(), restored_project.path())
        .args([
            "restore-agents",
            "-i",
            archive.to_str().unwrap(),
            "--agents",
            "qoder",
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    for (path, body) in [
        (
            restored_home.path().join(".qoder/settings.json"),
            fixtures[0].1,
        ),
        (
            restored_home.path().join(".agents/skills/shared/SKILL.md"),
            fixtures[1].1,
        ),
        (restored_project.path().join("AGENTS.md"), fixtures[2].1),
    ] {
        assert_eq!(fs::read_to_string(path).unwrap(), body);
    }
}

#[test]
fn backup_agents_sanitizes_secrets_by_default() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let claude_settings = home.path().join(".claude/settings.json");
    fs::create_dir_all(claude_settings.parent().unwrap()).unwrap();
    fs::write(
        &claude_settings,
        r#"{"mcpServers":{"remote":{"env":{"GITHUB_TOKEN":"ghp_FAKEfakeFAKEfakeFAKEfake012345678"}}}}"#,
    )
    .unwrap();

    let backup_tar = project.path().join("sanitized.tar.gz");

    let output = command_with_env(home.path(), project.path())
        .args(["backup-agents", "-o", backup_tar.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&backup_tar).unwrap().permissions().mode() & 0o777,
            0o600,
            "even sanitized archives contain verbatim active content and must be private"
        );
    }

    // Restore to clean sandbox
    let restore_home = tempfile::tempdir().unwrap();
    let restore_project = tempfile::tempdir().unwrap();

    let restore = command_with_env(restore_home.path(), restore_project.path())
        .args([
            "restore-agents",
            "-i",
            backup_tar.to_str().unwrap(),
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(restore.status.success());

    let restored = fs::read_to_string(restore_home.path().join(".claude/settings.json")).unwrap();
    assert!(
        !restored.contains("ghp_FAKEfakeFAKEfakeFAKEfake012345678"),
        "raw secret token must be redacted"
    );
    assert!(
        restored.contains("[REDACTED:"),
        "must contain redaction label, got: {restored}"
    );
}

#[test]
fn restore_agents_filters_by_agent_name() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let claude_settings = home.path().join(".claude/settings.json");
    fs::create_dir_all(claude_settings.parent().unwrap()).unwrap();
    fs::write(&claude_settings, r#"{"mcpServers":{"claude":{}}}"#).unwrap();

    let codex_config = home.path().join(".codex/config.toml");
    fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
    fs::write(&codex_config, "[mcp_servers.codex]\n").unwrap();

    let backup_tar = project.path().join("multi_agent.tar.gz");

    let backup_res = command_with_env(home.path(), project.path())
        .args(["backup-agents", "-o", backup_tar.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(backup_res.status.success());

    // Restore ONLY claude
    let restore_home = tempfile::tempdir().unwrap();
    let restore_project = tempfile::tempdir().unwrap();

    let restore_res = command_with_env(restore_home.path(), restore_project.path())
        .args([
            "restore-agents",
            "-i",
            backup_tar.to_str().unwrap(),
            "--agents",
            "claude",
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(restore_res.status.success());

    // Claude restored, Codex NOT restored
    assert!(restore_home.path().join(".claude/settings.json").exists());
    assert!(!restore_home.path().join(".codex/config.toml").exists());
}

#[test]
fn restore_agents_rejects_path_traversal() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let evil_tar = project.path().join("evil.tar.gz");
    {
        let file = std::fs::File::create(&evil_tar).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);

        let manifest = r#"{
            "version": 1,
            "created_at": "2026-09-28T00:00:00Z",
            "host": { "os": "macos", "arch": "aarch64" },
            "sanitized": true,
            "entries": [{
                "agent": "claude-code",
                "asset_kind": "mcp-config",
                "scope": "global",
                "archive_path": "evil.txt",
                "target_relative": "../../../etc/passwd"
            }]
        }"#;

        let mut h_m = tar::Header::new_gnu();
        h_m.set_size(manifest.len() as u64);
        h_m.set_cksum();
        tar.append_data(&mut h_m, "manifest.json", std::io::Cursor::new(manifest))
            .unwrap();

        let evil_body = "malicious payload";
        let mut h_b = tar::Header::new_gnu();
        h_b.set_size(evil_body.len() as u64);
        h_b.set_cksum();
        tar.append_data(&mut h_b, "evil.txt", std::io::Cursor::new(evil_body))
            .unwrap();

        tar.finish().unwrap();
    }

    let restore = command_with_env(home.path(), project.path())
        .args([
            "restore-agents",
            "-i",
            evil_tar.to_str().unwrap(),
            "--apply",
        ])
        .output()
        .unwrap();

    assert!(
        !restore.status.success(),
        "must fail on path traversal payload"
    );
    let err = String::from_utf8_lossy(&restore.stderr);
    assert!(err.contains("dangerous") || err.contains("traversal") || err.contains("malformed"));
}

#[test]
fn restore_agents_rejects_unauthorized_destination_path() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let evil_tar = project.path().join("evil_target.tar.gz");
    {
        let file = std::fs::File::create(&evil_tar).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);

        let manifest = r#"{
            "version": 1,
            "created_at": "2026-09-28T00:00:00Z",
            "host": { "os": "macos", "arch": "aarch64" },
            "sanitized": true,
            "entries": [{
                "agent": "claude-code",
                "asset_kind": "mcp-config",
                "scope": "project",
                "archive_path": "payload.txt",
                "target_relative": ".git/hooks/pre-commit"
            }]
        }"#;

        let mut h_m = tar::Header::new_gnu();
        h_m.set_size(manifest.len() as u64);
        h_m.set_cksum();
        tar.append_data(&mut h_m, "manifest.json", std::io::Cursor::new(manifest))
            .unwrap();

        let evil_body = "#!/bin/sh\necho malicious";
        let mut h_b = tar::Header::new_gnu();
        h_b.set_size(evil_body.len() as u64);
        h_b.set_cksum();
        tar.append_data(&mut h_b, "payload.txt", std::io::Cursor::new(evil_body))
            .unwrap();

        tar.finish().unwrap();
    }

    let restore = command_with_env(home.path(), project.path())
        .args([
            "restore-agents",
            "-i",
            evil_tar.to_str().unwrap(),
            "--apply",
        ])
        .output()
        .unwrap();

    assert!(
        !restore.status.success(),
        "must reject unauthorized destination path"
    );
    let err = String::from_utf8_lossy(&restore.stderr);
    assert!(err.contains("unauthorized path"));
}

#[test]
fn restore_agents_binary_safe_preserves_raw_bytes() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let plugin_dir = home.path().join(".claude/plugins/test-plugin");
    fs::create_dir_all(&plugin_dir).unwrap();
    let binary_bytes: Vec<u8> = vec![0xFF, 0xFE, 0x00, 0x01, 0x80, 0xC0, 0xDF, 0x80];
    let binary_file = plugin_dir.join("asset.bin");
    fs::write(&binary_file, &binary_bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&binary_file, fs::Permissions::from_mode(0o750)).unwrap();
    }

    let backup_tar = project.path().join("binary_test.tar.gz");

    let backup = command_with_env(home.path(), project.path())
        .args(["backup-agents", "-o", backup_tar.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(backup.status.success());

    let restore_home = tempfile::tempdir().unwrap();
    let restore_project = tempfile::tempdir().unwrap();

    let restore = command_with_env(restore_home.path(), restore_project.path())
        .args([
            "restore-agents",
            "-i",
            backup_tar.to_str().unwrap(),
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );

    let restored_bin = fs::read(
        restore_home
            .path()
            .join(".claude/plugins/test-plugin/asset.bin"),
    )
    .unwrap();
    assert_eq!(
        restored_bin, binary_bytes,
        "binary bytes must be preserved exactly"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(
            restore_home
                .path()
                .join(".claude/plugins/test-plugin/asset.bin"),
        )
        .unwrap()
        .permissions()
        .mode()
            & 0o777;
        assert_eq!(mode, 0o750, "portable executable bits must be restored");
    }
}

#[test]
#[cfg(unix)]
fn restore_agents_checks_permissions_before_skipping_identical_content() {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let plugin_dir = home.path().join(".claude/plugins/test-plugin");
    fs::create_dir_all(&plugin_dir).unwrap();
    let script = plugin_dir.join("run.sh");
    let body = b"#!/bin/sh\nexit 0\n";
    fs::write(&script, body).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o750)).unwrap();
    let mode = || fs::metadata(&script).unwrap().permissions().mode() & 0o777;

    let archive = project.path().join("backup.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .args([
            "backup-agents",
            "-o",
            archive.to_str().unwrap(),
            "--agents",
            "claude-code",
            "--scope",
            "global",
        ])
        .output()
        .unwrap();
    assert!(backup.status.success());

    fs::set_permissions(&script, fs::Permissions::from_mode(0o640)).unwrap();
    let preview = command_with_env(home.path(), project.path())
        .args(["restore-agents", "-i", archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(preview.status.success());
    assert!(String::from_utf8_lossy(&preview.stdout).contains("[OVERWRITE]"));
    assert_eq!(mode(), 0o640, "preview must not restore permissions");
    assert_eq!(fs::read_dir(&plugin_dir).unwrap().count(), 1);

    let without_force = command_with_env(home.path(), project.path())
        .args(["restore-agents", "-i", archive.to_str().unwrap(), "--apply"])
        .output()
        .unwrap();
    assert!(without_force.status.success());
    assert!(String::from_utf8_lossy(&without_force.stdout).contains("[SKIP]"));
    assert_eq!(mode(), 0o640, "permission changes still require --force");
    assert_eq!(fs::read_dir(&plugin_dir).unwrap().count(), 1);

    let restore = command_with_env(home.path(), project.path())
        .args([
            "restore-agents",
            "-i",
            archive.to_str().unwrap(),
            "--apply",
            "--force",
        ])
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_eq!(mode(), 0o750, "identical bytes must not hide mode changes");
    assert_eq!(fs::read(&script).unwrap(), body);
    let backup_path = fs::read_dir(&plugin_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("run.sh.bak-"))
        })
        .expect("permission restore must keep a backup copy");
    assert_eq!(fs::read(&backup_path).unwrap(), body);
    assert_eq!(
        fs::metadata(&backup_path).unwrap().permissions().mode() & 0o777,
        0o640
    );

    let identical = command_with_env(home.path(), project.path())
        .args([
            "restore-agents",
            "-i",
            archive.to_str().unwrap(),
            "--apply",
            "--force",
        ])
        .output()
        .unwrap();
    assert!(identical.status.success());
    assert!(String::from_utf8_lossy(&identical.stdout).contains("already identical"));
    assert_eq!(mode(), 0o750);
    assert_eq!(fs::read_dir(&plugin_dir).unwrap().count(), 2);
}

#[test]
#[cfg(unix)]
fn restore_agents_without_archived_permissions_keeps_identical_file_mode() {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let body = b"# instructions";
    let target = project.path().join("CLAUDE.md");
    fs::write(&target, body).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o750)).unwrap();
    let archive = project.path().join("without-mode.tar.gz");
    write_agent_archive(
        &archive,
        serde_json::json!([{
            "agent": "claude-code",
            "asset_kind": "instruction",
            "scope": "project",
            "archive_path": "project/CLAUDE.md",
            "target_relative": "CLAUDE.md",
            "sha256": sha256(body),
        }]),
        &[("project/CLAUDE.md", body)],
    );

    let restore = command_with_env(home.path(), project.path())
        .args([
            "restore-agents",
            "-i",
            archive.to_str().unwrap(),
            "--apply",
            "--force",
        ])
        .output()
        .unwrap();
    assert!(restore.status.success());
    assert!(String::from_utf8_lossy(&restore.stdout).contains("already identical"));
    assert_eq!(fs::read(&target).unwrap(), body);
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o750
    );
    assert_eq!(fs::read_dir(project.path()).unwrap().count(), 2);
}

#[test]
fn restore_agents_verifies_sha256_integrity() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let tampered_tar = project.path().join("tampered.tar.gz");
    {
        let file = std::fs::File::create(&tampered_tar).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);

        let manifest = r#"{
            "version": 1,
            "created_at": "2026-09-28T00:00:00Z",
            "host": { "os": "macos", "arch": "aarch64" },
            "sanitized": true,
            "entries": [{
                "agent": "claude-code",
                "asset_kind": "instruction",
                "scope": "project",
                "archive_path": "project/CLAUDE.md",
                "target_relative": "CLAUDE.md",
                "sha256": "0000000000000000000000000000000000000000000000000000000000000000"
            }]
        }"#;

        let mut h_m = tar::Header::new_gnu();
        h_m.set_size(manifest.len() as u64);
        h_m.set_cksum();
        tar.append_data(&mut h_m, "manifest.json", std::io::Cursor::new(manifest))
            .unwrap();

        let body = "tampered instruction content";
        let mut h_b = tar::Header::new_gnu();
        h_b.set_size(body.len() as u64);
        h_b.set_cksum();
        tar.append_data(&mut h_b, "project/CLAUDE.md", std::io::Cursor::new(body))
            .unwrap();

        tar.finish().unwrap();
    }

    let restore = command_with_env(home.path(), project.path())
        .args([
            "restore-agents",
            "-i",
            tampered_tar.to_str().unwrap(),
            "--apply",
        ])
        .output()
        .unwrap();

    assert!(
        !restore.status.success(),
        "must fail on SHA-256 integrity mismatch"
    );
    let err = String::from_utf8_lossy(&restore.stderr);
    assert!(err.contains("integrity mismatch") || err.contains("SHA-256"));
}

#[test]
fn backup_and_restore_kebab_case_agent_filter() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let claude_settings = home.path().join(".claude/settings.json");
    fs::create_dir_all(claude_settings.parent().unwrap()).unwrap();
    fs::write(&claude_settings, r#"{"mcpServers":{"claude":{}}}"#).unwrap();

    let codex_config = home.path().join(".codex/config.toml");
    fs::create_dir_all(codex_config.parent().unwrap()).unwrap();
    fs::write(&codex_config, "[mcp_servers.codex]\n").unwrap();

    let backup_tar = project.path().join("kebab_filter.tar.gz");

    // Filter by kebab-case wire string "claude-code"
    let backup_res = command_with_env(home.path(), project.path())
        .args([
            "backup-agents",
            "-o",
            backup_tar.to_str().unwrap(),
            "--agents",
            "claude-code",
        ])
        .output()
        .unwrap();
    assert!(backup_res.status.success());

    let restore_home = tempfile::tempdir().unwrap();
    let restore_project = tempfile::tempdir().unwrap();

    let restore_res = command_with_env(restore_home.path(), restore_project.path())
        .args([
            "restore-agents",
            "-i",
            backup_tar.to_str().unwrap(),
            "--agents",
            "claude-code",
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(restore_res.status.success());

    assert!(restore_home.path().join(".claude/settings.json").exists());
    assert!(!restore_home.path().join(".codex/config.toml").exists());
}

#[test]
fn restore_agents_force_keeps_previous_file_backup() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let source = home.path().join(".claude/settings.json");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::write(&source, "restored config").unwrap();

    let archive = project.path().join("backup.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .args([
            "backup-agents",
            "-o",
            archive.to_str().unwrap(),
            "--agents",
            "claude-code",
            "--include-secrets",
        ])
        .output()
        .unwrap();
    assert!(backup.status.success());

    let restore_home = tempfile::tempdir().unwrap();
    let restore_project = tempfile::tempdir().unwrap();
    let existing = restore_home.path().join(".claude/settings.json");
    fs::create_dir_all(existing.parent().unwrap()).unwrap();
    fs::write(&existing, "previous config").unwrap();

    let restore = command_with_env(restore_home.path(), restore_project.path())
        .args([
            "restore-agents",
            "-i",
            archive.to_str().unwrap(),
            "--apply",
            "--force",
        ])
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );
    assert_eq!(fs::read(&existing).unwrap(), b"restored config");

    let backup_path = fs::read_dir(existing.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("settings.json.bak-"))
        })
        .expect("overwrite must keep a backup copy");
    assert_eq!(fs::read(backup_path).unwrap(), b"previous config");
}

#[test]
fn restore_agents_requires_a_complete_one_to_one_manifest() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let body = b"# instructions";
    let entry = serde_json::json!({
        "agent": "claude-code",
        "asset_kind": "instruction",
        "scope": "project",
        "archive_path": "project/CLAUDE.md",
        "target_relative": "CLAUDE.md",
        "sha256": sha256(body),
    });

    let missing = project.path().join("missing.tar.gz");
    write_agent_archive(&missing, serde_json::json!([entry.clone()]), &[]);
    let output = command_with_env(home.path(), project.path())
        .args(["restore-agents", "-i", missing.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing asset"));

    let hidden = project.path().join("hidden.tar.gz");
    write_agent_archive(
        &hidden,
        serde_json::json!([]),
        &[("project/CLAUDE.md", body)],
    );
    let output = command_with_env(home.path(), project.path())
        .args(["restore-agents", "-i", hidden.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unmanifested asset"));

    let duplicate = project.path().join("duplicate.tar.gz");
    let second = serde_json::json!({
        "agent": "codex",
        "asset_kind": "instruction",
        "scope": "project",
        "archive_path": "project/AGENTS-as-CLAUDE.md",
        "target_relative": "CLAUDE.md",
        "sha256": sha256(body),
    });
    write_agent_archive(
        &duplicate,
        serde_json::json!([entry, second]),
        &[
            ("project/CLAUDE.md", body),
            ("project/AGENTS-as-CLAUDE.md", body),
        ],
    );
    let output = command_with_env(home.path(), project.path())
        .args(["restore-agents", "-i", duplicate.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("more than one asset"));
}

#[cfg(unix)]
#[test]
fn restore_agents_rejects_a_symlinked_destination_parent() {
    use std::os::unix::fs::symlink;

    let _guard = cli_test_lock();
    for (agent, dir, name) in [
        ("claude-code", ".claude", "settings.json"),
        ("kimi-code", ".kimi-code", "mcp.json"),
    ] {
        let source_home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let asset_dir = source_home.path().join(dir);
        fs::create_dir_all(&asset_dir).unwrap();
        fs::write(asset_dir.join(name), br#"{"mcpServers":{}}"#).unwrap();
        let archive = project.path().join("backup.tar.gz");
        let backup = command_with_env(source_home.path(), project.path())
            .args(backup_agents_args(&archive, agent))
            .output()
            .unwrap();
        assert_success(&backup, "backup");

        let restore_home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), restore_home.path().join(dir)).unwrap();
        let restore = command_with_env(restore_home.path(), project.path())
            .args(restore_agents_args(&archive, agent))
            .output()
            .unwrap();

        assert!(!restore.status.success());
        assert!(String::from_utf8_lossy(&restore.stderr).contains("symlinked path"));
        assert!(!outside.path().join(name).exists());
    }
}

#[test]
fn restore_agents_rejects_oversized_entries_from_the_header() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let archive_path = project.path().join("oversized.tar.gz");
    let file = fs::File::create(&archive_path).unwrap();
    let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    let mut header = tar::Header::new_gnu();
    header.set_path("oversized.bin").unwrap();
    header.set_size(128 * 1024 * 1024 + 1);
    header.set_mode(0o600);
    header.set_cksum();
    encoder.write_all(header.as_bytes()).unwrap();
    encoder.finish().unwrap();

    let output = command_with_env(home.path(), project.path())
        .args(["restore-agents", "-i", archive_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("exceeds"));
}

#[test]
fn agent_filters_are_exact_instead_of_substring_matches() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let settings = home.path().join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    fs::write(&settings, br#"{"mcpServers":{}}"#).unwrap();
    let archive = project.path().join("backup.tar.gz");

    let output = command_with_env(home.path(), project.path())
        .args([
            "backup-agents",
            "-o",
            archive.to_str().unwrap(),
            "--agents",
            "code",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        !archive.exists(),
        "ambiguous substring must not select Claude Code"
    );
}

#[cfg(unix)]
#[test]
fn backup_agents_does_not_follow_config_or_asset_root_symlinks() {
    use std::os::unix::fs::symlink;

    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("settings.json");
    fs::write(&secret, br#"{"token":"must-not-leak"}"#).unwrap();
    fs::create_dir_all(home.path().join(".claude")).unwrap();
    symlink(&secret, home.path().join(".claude/settings.json")).unwrap();
    let external_plugins = outside.path().join("plugins");
    fs::create_dir_all(&external_plugins).unwrap();
    fs::write(external_plugins.join("payload"), b"outside plugin").unwrap();
    symlink(&external_plugins, home.path().join(".claude/plugins")).unwrap();
    let external_skills = outside.path().join("skills");
    fs::create_dir_all(&external_skills).unwrap();
    fs::write(
        external_skills.join("SKILL.md"),
        b"outside Antigravity skill",
    )
    .unwrap();
    fs::create_dir_all(home.path().join(".gemini/config")).unwrap();
    symlink(&external_skills, home.path().join(".gemini/config/skills")).unwrap();
    let archive = project.path().join("backup.tar.gz");

    let output = command_with_env(home.path(), project.path())
        .args(["backup-agents", "-o", archive.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(
        !archive.exists(),
        "symlink-only Claude and Antigravity assets must not be archived"
    );
}

/// A plugin's manifest lives in a dot-directory (`.claude-plugin`,
/// `.codex-plugin`); skipping it with the other dot-directories archived
/// plugins and marketplaces that no longer load after a restore.
#[test]
fn backup_agents_keeps_plugin_manifest_directories() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let plugins = home.path().join(".claude/plugins/marketplaces/acme");
    for (relative, body) in [
        (".claude-plugin/marketplace.json", "{}"),
        ("plugins/demo/.claude-plugin/plugin.json", "{}"),
        ("plugins/demo/skills/demo/SKILL.md", "# Demo"),
        (".git/HEAD", "ref: refs/heads/main"),
    ] {
        let path = plugins.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }
    let codex_manifest = home
        .path()
        .join(".codex/plugins/demo/.codex-plugin/plugin.json");
    fs::create_dir_all(codex_manifest.parent().unwrap()).unwrap();
    fs::write(&codex_manifest, "{}").unwrap();
    let archive = project.path().join("backup.tar.gz");

    let output = command_with_env(home.path(), project.path())
        .args(["backup-agents", "-o", archive.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "backup-agents failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(
        fs::File::open(&archive).unwrap(),
    ));
    let paths: Vec<String> = tar
        .entries()
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    for expected in [
        "claude/plugins/marketplaces/acme/.claude-plugin/marketplace.json",
        "claude/plugins/marketplaces/acme/plugins/demo/.claude-plugin/plugin.json",
        "claude/plugins/marketplaces/acme/plugins/demo/skills/demo/SKILL.md",
        "codex/plugins/demo/.codex-plugin/plugin.json",
    ] {
        assert!(
            paths.iter().any(|path| path == expected),
            "missing {expected} in {paths:?}"
        );
    }
    assert!(
        !paths.iter().any(|path| path.contains("/.git/")),
        "VCS state must stay out of the archive: {paths:?}"
    );
}

#[test]
fn failed_backup_leaves_the_previous_archive_intact() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let plugin = home.path().join(".claude/plugins/huge.bin");
    fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    fs::File::create(&plugin)
        .unwrap()
        .set_len(128 * 1024 * 1024 + 1)
        .unwrap();
    let archive = project.path().join("backup.tar.gz");
    fs::write(&archive, b"previous valid archive placeholder").unwrap();

    let output = command_with_env(home.path(), project.path())
        .args(["backup-agents", "-o", archive.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("per-asset limit"));
    assert_eq!(
        fs::read(archive).unwrap(),
        b"previous valid archive placeholder"
    );
}

fn read_archive_entries_bytes(archive_path: &Path) -> Vec<(String, Vec<u8>)> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(
        fs::File::open(archive_path).unwrap(),
    ));
    tar.entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let path = entry.path().unwrap().to_string_lossy().into_owned();
            let mut buf = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut buf).unwrap();
            (path, buf)
        })
        .collect()
}

#[test]
fn backup_and_restore_redacts_toml_json_and_jsonc_without_corrupting_syntax() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let codex_secret = "fixture-only-abcdef00112233445566778899";
    let grok_secret = "fixture-only-grok00112233445566778899aa";
    let grok_comment_secret = "fixture-only-comment112233445566778899";
    let bearer_secret = "fixture-only-bearer00112233445566778899";
    let openai_secret = "fixture-only-openai00112233445566778899";
    let bare_token_secret = "FAKEfake0123456789ABCDEF0123456789";
    let jsonc_header_secret = "fixture-only-jsonc00112233445566778899aa";
    let jsonc_comment_secret = "fixture-only-jcomment112233445566778899";
    let jsonc_block_secret = "fixture-only-jblock112233445566778899aa";

    // 1. Codex .codex/config.toml (TOML with X-API-Key header + non-secret url)
    let codex_toml = home.path().join(".codex/config.toml");
    fs::create_dir_all(codex_toml.parent().unwrap()).unwrap();
    fs::write(
        &codex_toml,
        format!(
            "[mcp_servers.ai-memory]\n\
             url = \"http://127.0.0.1:49374/mcp\"\n\
             \n\
             [mcp_servers.ai-memory.http_headers]\n\
             X-API-Key = \"{codex_secret}\"\n"
        ),
    )
    .unwrap();

    // 2. Grok .grok/config.toml (TOML with api_key + commented-out secret)
    let grok_toml = home.path().join(".grok/config.toml");
    fs::create_dir_all(grok_toml.parent().unwrap()).unwrap();
    fs::write(
        &grok_toml,
        format!(
            "# X-API-Key = \"{grok_comment_secret}\"\n\
             api_key = \"{grok_secret}\"\n\
             model = \"grok-code\"\n"
        ),
    )
    .unwrap();

    // 3. JSON configs (.claude/settings.json, .cursor/mcp.json, .codex/mcp.json)
    let claude_json = home.path().join(".claude/settings.json");
    fs::create_dir_all(claude_json.parent().unwrap()).unwrap();
    fs::write(
        &claude_json,
        format!(
            r#"{{"mcpServers":{{"ai-memory":{{"url":"http://127.0.0.1:49374/mcp","headers":{{"X-API-Key":"{codex_secret}","Authorization":"Bearer {bearer_secret}"}}}}}}}}"#
        ),
    )
    .unwrap();

    let cursor_json = home.path().join(".cursor/mcp.json");
    fs::create_dir_all(cursor_json.parent().unwrap()).unwrap();
    fs::write(
        &cursor_json,
        format!(
            r#"{{"mcpServers":{{"remote":{{"command":"npx","env":{{"OPENAI_API_KEY":"{openai_secret}","LOG_LEVEL":"info"}}}}}}}}"#
        ),
    )
    .unwrap();

    let codex_json = home.path().join(".codex/mcp.json");
    fs::write(
        &codex_json,
        format!(
            r#"{{"mcpServers":{{"vault":{{"url":"https://vault.example/mcp","token":"{bare_token_secret}"}}}}}}"#
        ),
    )
    .unwrap();

    // 4. JSONC config (.config/opencode/opencode.jsonc) with line + block comments and trailing comma
    let opencode_jsonc = home.path().join(".config/opencode/opencode.jsonc");
    fs::create_dir_all(opencode_jsonc.parent().unwrap()).unwrap();
    fs::write(
        &opencode_jsonc,
        format!(
            "{{\n\
             // X-API-Key: {jsonc_comment_secret}\n\
             /* api_key = \"{jsonc_block_secret}\" */\n\
             \"mcp\": {{\n\
               \"ai-memory\": {{\n\
                 \"url\": \"http://127.0.0.1:49374/mcp\",\n\
                 \"headers\": {{\n\
                   \"X-API-Key\": \"{jsonc_header_secret}\",\n\
                 }},\n\
               }},\n\
             }},\n\
             }}\n"
        ),
    )
    .unwrap();

    let backup_tar = project.path().join("structured-sanitized.tar.gz");
    let backup = command_with_env(home.path(), project.path())
        .args(["backup-agents", "-o", backup_tar.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        backup.status.success(),
        "backup-agents failed: {}",
        String::from_utf8_lossy(&backup.stderr)
    );

    // Every raw secret must be completely absent from every archived entry.
    let all_secrets = [
        codex_secret,
        grok_secret,
        grok_comment_secret,
        bearer_secret,
        openai_secret,
        bare_token_secret,
        jsonc_header_secret,
        jsonc_comment_secret,
        jsonc_block_secret,
    ];
    for (entry_path, bytes) in read_archive_entries_bytes(&backup_tar) {
        let text = String::from_utf8_lossy(&bytes);
        for secret in all_secrets {
            assert!(
                !text.contains(secret),
                "raw secret `{secret}` leaked into archived entry {entry_path}"
            );
        }
    }

    // Restore into a clean sandbox.
    let restore_home = tempfile::tempdir().unwrap();
    let restore_project = tempfile::tempdir().unwrap();
    let restore = command_with_env(restore_home.path(), restore_project.path())
        .args([
            "restore-agents",
            "-i",
            backup_tar.to_str().unwrap(),
            "--apply",
        ])
        .output()
        .unwrap();
    assert!(
        restore.status.success(),
        "restore-agents failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );

    // Verify Codex TOML parses cleanly and preserves non-secret fields + redacted key.
    let restored_codex_toml =
        fs::read_to_string(restore_home.path().join(".codex/config.toml")).unwrap();
    let codex_doc: toml_edit::DocumentMut = restored_codex_toml
        .parse()
        .unwrap_or_else(|e| panic!("restored Codex TOML is invalid: {e}\n{restored_codex_toml}"));
    assert_eq!(
        codex_doc["mcp_servers"]["ai-memory"]["url"].as_str(),
        Some("http://127.0.0.1:49374/mcp")
    );
    let codex_key = codex_doc["mcp_servers"]["ai-memory"]["http_headers"]["X-API-Key"]
        .as_str()
        .expect("X-API-Key must remain a valid TOML string");
    assert!(
        codex_key.starts_with("[REDACTED:"),
        "expected redacted marker, got: {codex_key}"
    );

    // Verify Grok TOML parses cleanly and redacts both value and comment.
    let restored_grok_toml =
        fs::read_to_string(restore_home.path().join(".grok/config.toml")).unwrap();
    let grok_doc: toml_edit::DocumentMut = restored_grok_toml
        .parse()
        .unwrap_or_else(|e| panic!("restored Grok TOML is invalid: {e}\n{restored_grok_toml}"));
    assert_eq!(grok_doc["model"].as_str(), Some("grok-code"));
    let grok_key = grok_doc["api_key"]
        .as_str()
        .expect("api_key must remain a valid TOML string");
    assert!(
        grok_key.starts_with("[REDACTED:"),
        "expected redacted marker, got: {grok_key}"
    );
    assert!(!restored_grok_toml.contains(grok_comment_secret));
    assert!(
        restored_grok_toml.lines().next().unwrap().starts_with('#'),
        "comment line must keep `#` prefix: {restored_grok_toml}"
    );

    // Verify Claude JSON parses cleanly and preserves structure + non-secret url.
    let restored_claude =
        fs::read_to_string(restore_home.path().join(".claude/settings.json")).unwrap();
    let claude_val: serde_json::Value = serde_json::from_str(&restored_claude)
        .unwrap_or_else(|e| panic!("restored Claude JSON is invalid: {e}\n{restored_claude}"));
    assert_eq!(
        claude_val["mcpServers"]["ai-memory"]["url"].as_str(),
        Some("http://127.0.0.1:49374/mcp")
    );
    assert!(
        claude_val["mcpServers"]["ai-memory"]["headers"]["X-API-Key"]
            .as_str()
            .unwrap()
            .contains("[REDACTED:")
    );
    assert!(
        claude_val["mcpServers"]["ai-memory"]["headers"]["Authorization"]
            .as_str()
            .unwrap()
            .contains("[REDACTED:")
    );

    // Verify Cursor JSON parses cleanly.
    let restored_cursor = fs::read_to_string(restore_home.path().join(".cursor/mcp.json")).unwrap();
    let cursor_val: serde_json::Value = serde_json::from_str(&restored_cursor)
        .unwrap_or_else(|e| panic!("restored Cursor JSON is invalid: {e}\n{restored_cursor}"));
    assert_eq!(
        cursor_val["mcpServers"]["remote"]["command"].as_str(),
        Some("npx")
    );
    assert_eq!(
        cursor_val["mcpServers"]["remote"]["env"]["LOG_LEVEL"].as_str(),
        Some("info")
    );
    assert!(
        cursor_val["mcpServers"]["remote"]["env"]["OPENAI_API_KEY"]
            .as_str()
            .unwrap()
            .contains("[REDACTED:")
    );

    // Verify Codex mcp.json parses cleanly and redacts bare 32+ char `token`.
    let restored_codex_json =
        fs::read_to_string(restore_home.path().join(".codex/mcp.json")).unwrap();
    let codex_json_val: serde_json::Value = serde_json::from_str(&restored_codex_json)
        .unwrap_or_else(|e| panic!("restored Codex JSON is invalid: {e}\n{restored_codex_json}"));
    assert_eq!(
        codex_json_val["mcpServers"]["vault"]["url"].as_str(),
        Some("https://vault.example/mcp")
    );
    assert!(
        codex_json_val["mcpServers"]["vault"]["token"]
            .as_str()
            .unwrap()
            .contains("[REDACTED:")
    );

    // Verify OpenCode JSONC parses cleanly and redacts values + comments.
    let restored_opencode =
        fs::read_to_string(restore_home.path().join(".config/opencode/opencode.jsonc")).unwrap();
    let opencode_ast = jsonc_parser::parse_to_ast(
        &restored_opencode,
        &jsonc_parser::CollectOptions::default(),
        &jsonc_parser::ParseOptions::default(),
    )
    .unwrap_or_else(|e| panic!("restored OpenCode JSONC is invalid: {e}\n{restored_opencode}"));
    let opencode_val: serde_json::Value = opencode_ast
        .value
        .map_or(serde_json::Value::Null, Into::into);
    assert_eq!(
        opencode_val["mcp"]["ai-memory"]["url"].as_str(),
        Some("http://127.0.0.1:49374/mcp")
    );
    assert!(
        opencode_val["mcp"]["ai-memory"]["headers"]["X-API-Key"]
            .as_str()
            .unwrap()
            .contains("[REDACTED:")
    );
    assert!(!restored_opencode.contains(jsonc_comment_secret));
    assert!(!restored_opencode.contains(jsonc_block_secret));
}

#[test]
fn backup_agents_include_secrets_and_malformed_config_fallback() {
    let _guard = cli_test_lock();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();

    let secret = "fixture-only-abcdef00112233445566778899";

    // Valid Codex TOML for --include-secrets control
    let codex_toml = home.path().join(".codex/config.toml");
    fs::create_dir_all(codex_toml.parent().unwrap()).unwrap();
    let raw_codex = format!("[mcp_servers.ai-memory.http_headers]\nX-API-Key = \"{secret}\"\n");
    fs::write(&codex_toml, &raw_codex).unwrap();

    // Malformed Grok TOML + malformed Claude JSON on disk
    let grok_toml = home.path().join(".grok/config.toml");
    fs::create_dir_all(grok_toml.parent().unwrap()).unwrap();
    fs::write(&grok_toml, format!("broken = [\napi_key = \"{secret}\"\n")).unwrap();

    let claude_json = home.path().join(".claude/settings.json");
    fs::create_dir_all(claude_json.parent().unwrap()).unwrap();
    fs::write(
        &claude_json,
        format!("{{\"broken\": , \"OPENAI_API_KEY\": \"{secret}\"}}"),
    )
    .unwrap();

    // 1. Default sanitized backup: malformed files must still succeed and redact secrets.
    let sanitized_tar = project.path().join("malformed-sanitized.tar.gz");
    let sanitized = command_with_env(home.path(), project.path())
        .args(["backup-agents", "-o", sanitized_tar.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        sanitized.status.success(),
        "backup-agents failed on malformed configs: {}",
        String::from_utf8_lossy(&sanitized.stderr)
    );
    for (entry_path, bytes) in read_archive_entries_bytes(&sanitized_tar) {
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(secret),
            "raw secret leaked into malformed fallback entry {entry_path}"
        );
    }

    // 2. --include-secrets control: raw bytes preserved verbatim.
    let raw_tar = project.path().join("include-secrets.tar.gz");
    let raw = command_with_env(home.path(), project.path())
        .args([
            "backup-agents",
            "-o",
            raw_tar.to_str().unwrap(),
            "--include-secrets",
        ])
        .output()
        .unwrap();
    assert!(raw.status.success());

    let restore_home = tempfile::tempdir().unwrap();
    let restore_project = tempfile::tempdir().unwrap();
    let restore = command_with_env(restore_home.path(), restore_project.path())
        .args(["restore-agents", "-i", raw_tar.to_str().unwrap(), "--apply"])
        .output()
        .unwrap();
    assert!(restore.status.success());
    assert_eq!(
        fs::read_to_string(restore_home.path().join(".codex/config.toml")).unwrap(),
        raw_codex
    );
}
