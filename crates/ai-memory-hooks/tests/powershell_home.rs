//! Windows PowerShell marker-boundary regression tests.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate should live under crates/ai-memory-hooks")
        .to_path_buf()
}

#[test]
fn marker_lookup_does_not_assign_to_powershell_home() {
    let temp = tempfile::tempdir().unwrap();
    let helper = repo_root()
        .join("hooks")
        .join("lib")
        .join("ai-memory-hook.ps1");
    let helper = helper.to_string_lossy().replace('\'', "''");
    let program = format!(
        ". '{helper}'; $Error.Clear(); \
         $null = Get-AiMemoryMarkerToml -Cwd $env:AI_MEMORY_TEST_CWD; \
         if ($Error.Count -ne 0) {{ \
             [Console]::Error.Write(($Error | Out-String)); exit 17 \
         }}; [Console]::Out.Write('ok')"
    );
    let output = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &program,
        ])
        .env("AI_MEMORY_TEST_CWD", temp.path())
        .output()
        .expect("run PowerShell marker lookup");
    assert!(
        output.status.success(),
        "PowerShell marker lookup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "PowerShell marker lookup polluted stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"ok");
}
