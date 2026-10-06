#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::Output;

use crate::e2e_support::hermetic;

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");

fn fake_opencode(home: &Path, version: &str) -> std::path::PathBuf {
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let executable = bin.join("opencode");
    std::fs::write(
        &executable,
        format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"),
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(executable, permissions).unwrap();
    bin
}

fn run(home: &Path, path: &Path, args: &[&str]) -> Output {
    hermetic(BIN)
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("PATH", path)
        .output()
        .unwrap()
}

#[test]
fn generic_install_commands_select_the_probed_major() {
    for (version, dialect, plugin_marker, mcp_marker, setup_marker) in [
        (
            "1.18.34",
            "v1",
            "export default AiMemoryHooks",
            "\"enabled\": true",
            "plugins/ai-memory.ts",
        ),
        (
            "opencode v2.0.23 (build abc)",
            "v2",
            "const AiMemoryOpencode2: Plugin.Plugin",
            "\"servers\"",
            "plugins/ai-memory-opencode2.ts",
        ),
    ] {
        let home = tempfile::tempdir().unwrap();
        let path = fake_opencode(home.path(), version);
        let hooks = run(
            home.path(),
            &path,
            &[
                "install-hooks",
                "--agent",
                "opencode",
                "--server-url",
                "http://127.0.0.1:49374",
            ],
        );
        assert!(
            hooks.status.success(),
            "{}",
            String::from_utf8_lossy(&hooks.stderr)
        );
        assert!(String::from_utf8_lossy(&hooks.stdout).contains(plugin_marker));

        let mcp = run(
            home.path(),
            &path,
            &[
                "install-mcp",
                "--client",
                "opencode",
                "--server-url",
                "http://127.0.0.1:49374",
            ],
        );
        assert!(
            mcp.status.success(),
            "{}",
            String::from_utf8_lossy(&mcp.stderr)
        );
        assert!(String::from_utf8_lossy(&mcp.stdout).contains(mcp_marker));

        let setup = run(
            home.path(),
            Path::new(""),
            &[
                "setup-agent",
                "--agent",
                "opencode",
                "--opencode-dialect",
                dialect,
                "--to",
                "unused",
            ],
        );
        assert!(
            setup.status.success(),
            "{}",
            String::from_utf8_lossy(&setup.stderr)
        );
        assert!(String::from_utf8_lossy(&setup.stdout).contains(setup_marker));
    }
}

#[test]
fn explicit_v2_aliases_do_not_require_an_executable_probe() {
    let home = tempfile::tempdir().unwrap();
    for alias in ["opencode2", "opencode-v2", "open-code2"] {
        let output = run(
            home.path(),
            Path::new(""),
            &[
                "install-mcp",
                "--client",
                alias,
                "--server-url",
                "http://127.0.0.1:49374",
            ],
        );
        assert!(
            output.status.success(),
            "{alias}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("\"servers\""));
    }
}

#[test]
fn setup_agent_alias_forces_v2_without_a_host_executable() {
    let home = tempfile::tempdir().unwrap();
    let output = run(
        home.path(),
        Path::new(""),
        &["setup-agent", "--agent", "opencode2", "--to", "unused"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("const AiMemoryOpencode2: Plugin.Plugin"));
    assert!(stdout.contains("\"servers\""));
}

#[test]
fn setup_agent_alias_rejects_a_conflicting_dialect_override() {
    let home = tempfile::tempdir().unwrap();
    let output = run(
        home.path(),
        Path::new(""),
        &[
            "setup-agent",
            "--agent",
            "opencode2",
            "--opencode-dialect",
            "v1",
            "--to",
            "unused",
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("cannot be combined with the force-V2 opencode2 compatibility alias")
    );
}

#[test]
fn setup_agent_generic_requires_an_explicit_dialect_without_probing() {
    let home = tempfile::tempdir().unwrap();
    let output = run(
        home.path(),
        Path::new(""),
        &["setup-agent", "--agent", "opencode", "--to", "unused"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires --opencode-dialect v1|v2"));
}

#[test]
fn malformed_generic_version_fails_without_writing_config() {
    let home = tempfile::tempdir().unwrap();
    let path = fake_opencode(home.path(), "OpenCode latest");
    let config = home.path().join("opencode.json");
    let output = run(
        home.path(),
        &path,
        &[
            "install-mcp",
            "--client",
            "opencode",
            "--apply",
            "--config-file",
            config.to_str().unwrap(),
        ],
    );
    assert!(!output.status.success());
    assert!(!config.exists());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not determine the OpenCode major")
    );
}
