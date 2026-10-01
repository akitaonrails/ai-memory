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
    let output = Command::new(ai_memory_test_support::powershell_exe())
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

/// Per case: the marker found from `org\repo` (`-` for none), whether `plain`
/// is routed, then `|`. The cases are `$HOME` as-is, with a trailing `\`, and
/// with a trailing `/`, then a control whose home is the fixture root.
const TRAILING_SEPARATOR_PROGRAM: &str = r#". '__HELPER__'; $Error.Clear()
$r = $env:AI_MEMORY_TEST_ROOT
$h = Join-Path $r 'home'
$repo = Join-Path (Join-Path $h 'org') 'repo'
$plain = Join-Path $h 'plain'
$out = foreach ($hm in @($h, ($h + '\'), ($h + '/'), $r)) {
    $env:HOME = $hm; $env:USERPROFILE = $hm
    $marker = Get-AiMemoryMarkerToml -Cwd $repo
    $found = if ($marker) { Split-Path (Split-Path $marker -Parent) -Leaf } else { '-' }
    $routed = if (Test-AiMemoryServerRouted -Cwd $plain) { 'T' } else { 'F' }
    "$found,$routed|"
}
if ($Error.Count -ne 0) { [Console]::Error.Write(($Error | Out-String)); exit 17 }
[Console]::Out.Write(($out -join ''))
"#;

/// A trailing separator on `$HOME` must not move the walk boundary: a marker
/// above home stays out of reach, and its `server` key routes nothing.
#[test]
fn a_trailing_separator_on_home_keeps_the_walk_boundary() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let write = |relative: &str, bytes: &[u8]| {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    // Above home: out of reach unless the control widens home to the root.
    write(
        ".ai-memory.toml",
        b"workspace = \"above\"\nserver = \"x\"\n",
    );
    // Inside home, above the checkout root: always in reach.
    write("home/org/.ai-memory.toml", b"workspace = \"org\"\n");
    std::fs::create_dir_all(root.join("home/org/repo/.git")).unwrap();
    std::fs::create_dir_all(root.join("home/plain")).unwrap();

    let helper = repo_root()
        .join("hooks")
        .join("lib")
        .join("ai-memory-hook.ps1");
    let helper = helper.to_string_lossy().replace('\'', "''");
    let program = TRAILING_SEPARATOR_PROGRAM.replace("__HELPER__", &helper);
    let output = Command::new(ai_memory_test_support::powershell_exe())
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &program,
        ])
        .env("AI_MEMORY_TEST_ROOT", root)
        .output()
        .expect("run PowerShell marker walks");
    assert!(
        output.status.success(),
        "PowerShell marker walks failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "org,F|org,F|org,F|org,T|",
        "home as-is, trailing `\\`, trailing `/`, then the root-home control"
    );
}
