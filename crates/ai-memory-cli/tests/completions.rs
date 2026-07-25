//! `ai-memory completions <shell>` output regression tests.
//!
//! The generator is clap_complete's, so these assert the contract the
//! *project* owns: a script actually lands on stdout, it is addressed to the
//! `ai-memory` binary name, it covers the real subcommand surface, and it is
//! produced without touching a data directory or config file.

use std::process::Command;

fn ai_memory_bin() -> std::path::PathBuf {
    // CARGO_BIN_EXE_<name> is set by cargo for integration tests.
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_ai-memory"))
}

fn completions_for(shell: &str) -> String {
    let temp = tempfile::tempdir().unwrap();
    let missing_data_dir = temp.path().join("missing-data");
    let out = Command::new(ai_memory_bin())
        // Point config + data dir at a path that does not exist: generating a
        // completion script must not read, create, or require either.
        .env("AI_MEMORY_DATA_DIR", &missing_data_dir)
        .args(["completions", shell])
        .output()
        .unwrap_or_else(|e| panic!("failed to run `completions {shell}`: {e}"));

    assert!(
        out.status.success(),
        "`completions {shell}` exited with {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        out.stderr.is_empty(),
        "`completions {shell}` polluted stderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !missing_data_dir.exists(),
        "`completions {shell}` created its configured data directory",
    );
    String::from_utf8(out.stdout).expect("completion script should be UTF-8")
}

#[test]
fn every_supported_shell_emits_a_script_naming_the_binary() {
    for shell in ["bash", "elvish", "fish", "powershell", "zsh"] {
        let script = completions_for(shell);
        assert!(
            !script.trim().is_empty(),
            "{shell} completion script was empty",
        );
        assert!(
            script.contains("ai-memory"),
            "{shell} completion script never mentions the binary name",
        );
    }
}

#[test]
fn scripts_cover_subcommands_from_across_the_tree() {
    // A spread of shallow, hyphenated, and nested commands: enough that a
    // regression in how the command tree is handed to the generator shows up.
    for shell in ["bash", "fish", "zsh"] {
        let script = completions_for(shell);
        for subcommand in [
            "init",
            "serve",
            "write-page",
            "purge-project",
            "user",
            "rotate-token",
            "auth",
            "login",
        ] {
            assert!(
                script.contains(subcommand),
                "{shell} completion script is missing `{subcommand}`",
            );
        }
    }
}

#[test]
fn an_unknown_shell_fails_without_emitting_a_script() {
    let out = Command::new(ai_memory_bin())
        .args(["completions", "nushell"])
        .output()
        .expect("failed to run completions");

    assert!(!out.status.success(), "unknown shell should be rejected");
    assert!(
        out.stdout.is_empty(),
        "nothing should reach stdout for an unknown shell",
    );
}
