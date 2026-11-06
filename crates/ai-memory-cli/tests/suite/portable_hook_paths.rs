//! Home-relative hook paths (#2035): `install-hooks` writes `"$HOME/..."` for
//! the binary and the data dir, and the opt-outs keep absolute paths.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::e2e_support::hermetic;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

/// The binary's parent directory, used as `HOME` so the exe renders as
/// `"$HOME/ai-memory"`.
fn binary_home() -> PathBuf {
    Path::new(binary()).parent().unwrap().to_path_buf()
}

fn install_hooks(
    config: &Path,
    data_dir: &Path,
    extra: &[&str],
    env: &[(&str, &str)],
) -> std::process::Output {
    let mut cmd = hermetic(binary());
    cmd.env("HOME", binary_home())
        .env("USERPROFILE", binary_home())
        .env("AI_MEMORY_DATA_DIR", data_dir)
        .args([
            "install-hooks",
            "--agent",
            "claude-code",
            "--apply",
            "--config-file",
        ])
        .arg(config);
    cmd.args(extra);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("run ai-memory install-hooks");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// Every `command` string in the settings file, in document order.
fn commands(config: &Path) -> Vec<String> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(config).unwrap()).unwrap();
    let mut found = Vec::new();
    collect_commands(&value, &mut found);
    found
}

fn collect_commands(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key == "command" {
                    if let Some(text) = child.as_str() {
                        found.push(text.to_string());
                    }
                } else {
                    collect_commands(child, found);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|item| collect_commands(item, found)),
        _ => {}
    }
}

fn ai_memory_hook_commands(config: &Path) -> Vec<String> {
    commands(config)
        .into_iter()
        .filter(|command| command.contains("hook --event"))
        .collect()
}

fn binary_file_name() -> String {
    Path::new(binary())
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// The `shell_quote` rule: bare when only `[A-Za-z0-9-_./:@%+=,]`, else single-quoted.
fn posix_quote(text: &str) -> String {
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=,".contains(c))
    {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// Every ai-memory hook must be exactly `<quoted exe> --data-dir <quoted data dir> hook --event …`.
fn assert_absolute_hook_commands(config: &Path, data_dir: &Path) {
    let hook_commands = ai_memory_hook_commands(config);
    assert!(!hook_commands.is_empty(), "no ai-memory hook written");
    let expected_start = format!(
        "{} --data-dir {} hook --event ",
        posix_quote(binary()),
        posix_quote(&data_dir.to_string_lossy()),
    );
    for command in &hook_commands {
        assert!(command.starts_with(&expected_start), "{command}");
    }
}

#[test]
fn install_hooks_writes_home_relative_exe() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("settings.json");
    install_hooks(&config, &dir.path().join("data"), &[], &[]);

    let hook_commands = ai_memory_hook_commands(&config);
    assert!(!hook_commands.is_empty(), "no ai-memory hook written");
    let expected_start = format!("\"$HOME/{}\" ", binary_file_name());
    for command in &hook_commands {
        assert!(command.starts_with(&expected_start), "{command}");
    }
}

#[test]
fn install_hooks_reapply_is_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("settings.json");
    let data_dir = dir.path().join("data");
    install_hooks(&config, &data_dir, &[], &[]);
    let first_bytes = std::fs::read(&config).unwrap();
    let first_count = ai_memory_hook_commands(&config).len();

    install_hooks(&config, &data_dir, &[], &[]);

    assert_eq!(std::fs::read(&config).unwrap(), first_bytes);
    assert_eq!(ai_memory_hook_commands(&config).len(), first_count);
}

#[test]
fn install_hooks_replaces_handwritten_home_command() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("settings.json");
    let handwritten = "$HOME/.local/share/ai-memory/native-runner/ai-memory --data-dir $HOME/.local/share/ai-memory hook --event stop --agent claude-code --server-url http://127.0.0.1:49374";
    let seeded = serde_json::json!({
        "hooks": {
            "Stop": [{ "hooks": [{ "type": "command", "command": handwritten }] }]
        }
    });
    std::fs::write(&config, serde_json::to_string_pretty(&seeded).unwrap()).unwrap();

    install_hooks(&config, &dir.path().join("data"), &[], &[]);

    let stop_commands: Vec<String> = ai_memory_hook_commands(&config)
        .into_iter()
        .filter(|command| command.contains("hook --event stop"))
        .collect();
    assert_eq!(stop_commands.len(), 1, "{stop_commands:?}");
    assert!(
        stop_commands[0].starts_with(&format!("\"$HOME/{}\" ", binary_file_name())),
        "{}",
        stop_commands[0]
    );
}

#[test]
fn install_hooks_absolute_paths_flag_writes_absolute_exe() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("settings.json");
    install_hooks(
        &config,
        &dir.path().join("data"),
        &["--absolute-paths"],
        &[],
    );

    assert_absolute_hook_commands(&config, &dir.path().canonicalize().unwrap().join("data"));
}

#[test]
fn install_hooks_env_opt_out_writes_absolute_exe() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("settings.json");
    install_hooks(
        &config,
        &dir.path().join("data"),
        &[],
        &[("AI_MEMORY_HOOK_ABSOLUTE_PATHS", "1")],
    );

    assert_absolute_hook_commands(&config, &dir.path().canonicalize().unwrap().join("data"));
}
