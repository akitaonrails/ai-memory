//! Integration tests for ai-memory managed routing skills.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ai-memory")
}

#[test]
fn install_skills_print_outputs_plan_without_mutating_target_dir() {
    let home = tempfile::tempdir().unwrap();
    let target = home.path().join("custom-skills");

    let output = Command::new(bin())
        .args([
            "install-skills",
            "--target-dir",
            target.to_str().unwrap(),
            "--print",
        ])
        .env("HOME", home.path())
        .env("AI_MEMORY_DATA_DIR", home.path().join(".ai-memory-data"))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "install-skills --print failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains(
            &target
                .join("ai-memory-retrieval/SKILL.md")
                .display()
                .to_string()
        )
    );
    assert!(stdout.contains("<!-- ai-memory-managed: routing-skill -->"));
    assert!(
        !target.exists(),
        "--print must not create the target directory"
    );
}
