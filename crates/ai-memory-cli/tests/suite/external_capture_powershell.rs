//! The PowerShell hook bundle must honour `AI_MEMORY_CAPTURE_OWNER` the same
//! way the POSIX one does: a non-blank owner suppresses the capture POST while
//! the handoff GET still runs and still reaches the agent through stdout.
//!
//! Windows-only. `hooks/lib/ai-memory-hook.ps1` is the shipped helper for every
//! PowerShell-hosted agent, and this is the one CI leg with a PowerShell to run
//! it under. The test dot-sources the REAL helper and shadows nothing but
//! `Invoke-WebRequest`, so the gate, the session/marker query building and the
//! stdout contract under test are the shipped code paths.

#[cfg(windows)]
mod windows_only {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;

    use serde_json::json;

    use crate::e2e_support::hermetic;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("crate should live under crates/ai-memory-cli")
            .to_path_buf()
    }

    /// Dot-sources the shipped helper, then defines `Invoke-WebRequest` in the
    /// script scope. A function outranks a cmdlet in PowerShell's command
    /// resolution, so every request the helper makes lands here instead of on
    /// the network; nothing else about the helper is replaced.
    const DRIVER: &str = r#"
$ErrorActionPreference = 'Stop'
. $env:AI_MEMORY_TEST_HELPER

function Invoke-WebRequest {
    param(
        [switch] $UseBasicParsing,
        [int] $TimeoutSec,
        [string] $Method = 'Get',
        [string] $Uri,
        $Headers,
        [string] $ContentType,
        $Body
    )
    Add-Content -LiteralPath $env:AI_MEMORY_TEST_LOG -Value ('{0} {1}' -f $Method.ToUpperInvariant(), $Uri)
    if ($Method -eq 'Post') { return $null }
    return [pscustomobject]@{ Content = 'HANDOFF FROM SERVER' }
}

Invoke-AiMemoryHook -Event 'session-start' -Agent 'claude-code' -FetchHandoff
"#;

    struct Run {
        requests: Vec<String>,
        stdout: String,
    }

    impl Run {
        fn methods(&self) -> Vec<&str> {
            self.requests
                .iter()
                .filter_map(|line| line.split_whitespace().next())
                .collect()
        }
    }

    /// Run the helper once under `owner`. `None` leaves the variable unset.
    ///
    /// The sandbox is kept, not dropped: these fixtures are small, recoverable
    /// and worth reading after a CI failure, and nothing here should delete a
    /// tree on its own.
    fn run(owner: Option<&str>) -> Run {
        let tmp = tempfile::tempdir().unwrap().keep();
        let driver = tmp.join("driver.ps1");
        let log = tmp.join("requests.log");
        std::fs::write(&driver, DRIVER).unwrap();
        std::fs::write(&log, "").unwrap();

        // `hermetic` drops every inherited AI_MEMORY_* variable, so only the
        // ones set back below reach the helper.
        let mut command = hermetic(ai_memory_test_support::powershell_exe());
        command
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&driver)
            .env(
                "AI_MEMORY_TEST_HELPER",
                repo_root().join("hooks/lib/ai-memory-hook.ps1"),
            )
            .env("AI_MEMORY_TEST_LOG", &log)
            // Every file the helper may touch (session id, briefed markers)
            // stays in the sandbox, and the marker walk starts and ends there
            // too, so no .ai-memory.toml outside the test can reach it.
            .env("AI_MEMORY_DATA_DIR", &tmp)
            .env("AI_MEMORY_HOME", &tmp)
            .env("HOME", &tmp)
            .env("USERPROFILE", &tmp)
            .current_dir(&tmp)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(owner) = owner {
            command.env("AI_MEMORY_CAPTURE_OWNER", owner);
        }

        let mut child = command.spawn().expect("spawn PowerShell");
        // Read-AiMemoryStdin only reads when stdin is redirected, which a pipe
        // makes true. The payload carries the sandbox as cwd, so the marker
        // walk resolves inside it.
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"session_id": "ps-owner-test", "cwd": tmp})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "driver failed for owner={owner:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        Run {
            requests: std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .map(str::to_owned)
                .filter(|line| !line.trim().is_empty())
                .collect(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        }
    }

    #[test]
    fn capture_owner_suppresses_the_post_and_still_delivers_the_handoff() {
        let run = run(Some("orchestrator-a"));
        assert_eq!(
            run.methods(),
            vec!["GET"],
            "an external owner must produce no capture POST: {:?}",
            run.requests
        );
        assert!(
            run.requests[0].contains("/handoff?agent=claude-code"),
            "the handoff GET must still be issued: {:?}",
            run.requests
        );
        assert!(
            run.stdout.contains("HANDOFF FROM SERVER"),
            "the fetched context must still reach the agent: {:?}",
            run.stdout
        );
    }

    #[test]
    fn blank_capture_owner_keeps_capture_on() {
        // An empty value may reach the child as "unset" on Windows; either way
        // the contract is the same, so both spellings assert the same thing.
        for owner in [None, Some(""), Some("  \t  ")] {
            let run = run(owner);
            assert_eq!(
                run.methods(),
                vec!["POST", "GET"],
                "owner={owner:?} must not suppress capture: {:?}",
                run.requests
            );
            assert!(
                run.requests[0].contains("/hook?event=session-start&agent=claude-code"),
                "owner={owner:?} POST target: {:?}",
                run.requests
            );
            assert!(
                run.stdout.contains("HANDOFF FROM SERVER"),
                "owner={owner:?} stdout: {:?}",
                run.stdout
            );
        }
    }
}
