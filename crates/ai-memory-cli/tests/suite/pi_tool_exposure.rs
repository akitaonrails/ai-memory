//! #1988: `pi_tool_exposure` reaches the generated Pi extension through the
//! real binary's `Config::load`, in preview and in `--apply`.

use std::path::Path;
use std::process::Output;

use crate::e2e_support::hermetic;

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
const CODEMODE: &str = "exposure: \"codemode\",";

fn install_hooks(root: &Path, config: &str, env: Option<&str>, apply: bool) -> Output {
    let config_path = root.join("config.toml");
    std::fs::write(&config_path, config).unwrap();
    let mut cmd = hermetic(BIN);
    cmd.arg("--config")
        .arg(&config_path)
        .args(["install-hooks", "--agent", "pi"])
        .args(["--server-url", "http://127.0.0.1:49374"])
        .env("HOME", root)
        .env("USERPROFILE", root)
        .env("AI_MEMORY_HOME", root)
        .env("AI_MEMORY_DATA_DIR", root.join("data"))
        .env("PI_CODING_AGENT_DIR", root.join("pi"))
        .current_dir(root);
    if apply {
        cmd.arg("--apply")
            .arg("--config-file")
            .arg(root.join("ai-memory-pi.ts"));
    }
    if let Some(value) = env {
        cmd.env("AI_MEMORY_PI_TOOL_EXPOSURE", value);
    }
    cmd.output().unwrap()
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "install-hooks failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn preview_and_apply_follow_pi_tool_exposure() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    let preview = stdout(&install_hooks(root, "", None, false));
    assert!(preview.contains("pi.registerTool"), "{preview}");
    assert!(!preview.contains("exposure"), "{preview}");

    let codemode = "pi_tool_exposure = \"codemode\"\n";
    let preview = stdout(&install_hooks(root, codemode, None, false));
    assert_eq!(preview.matches(CODEMODE).count(), 1, "{preview}");

    stdout(&install_hooks(root, codemode, None, true));
    let extension = root.join("ai-memory-pi.ts");
    let written = std::fs::read_to_string(&extension).unwrap();
    assert_eq!(written.matches(CODEMODE).count(), 1, "{written}");
    let again = stdout(&install_hooks(root, codemode, None, true));
    assert!(again.contains("already up to date"), "{again}");

    // The environment overrides the file, as for every top-level key.
    let preview = stdout(&install_hooks(root, codemode, Some("direct"), false));
    assert!(!preview.contains("exposure"), "{preview}");
    let preview = stdout(&install_hooks(root, "", Some("codemode"), false));
    assert!(preview.contains(CODEMODE), "{preview}");
}

#[test]
fn an_unknown_pi_tool_exposure_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let output = install_hooks(tmp.path(), "pi_tool_exposure = \"deferred\"\n", None, false);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("pi_tool_exposure"), "{stderr}");
}
