//! Real-binary coverage for the Kiro `--yolo` translation.
//!
//! The unit tests in `ai-memory-workstream::harness` pin where `apply_yolo`
//! puts `--trust-all-tools`. These tests ask the installed `kiro-cli` whether
//! it accepts that argv: clap reads `--help` only after every argument before
//! it parsed, so a trailing `--help` checks the placement without starting a
//! session or calling a model. They skip when `kiro-cli` does not run here,
//! and run in an empty home and project so no real Kiro state is read or
//! written.

use std::ffi::OsString;
use std::process::{Command, Output};

use ai_memory_workstream::{ManagedHarness, apply_yolo, build_launch_plan};

/// `Some` when an executable `kiro-cli` on `PATH` answers `--version`.
fn kiro_cli() -> Option<&'static str> {
    let runs = Command::new("kiro-cli")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success());
    if !runs {
        eprintln!("skipping: kiro-cli does not run here");
    }
    runs.then_some("kiro-cli")
}

fn run_kiro(kiro: &str, args: &[OsString]) -> Output {
    let temp = tempfile::tempdir().unwrap();
    let (home, project) = (temp.path().join("home"), temp.path().join("project"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    Command::new(kiro)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .current_dir(&project)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("run kiro-cli")
}

fn parse_with_help(kiro: &str, args: &[OsString]) -> Output {
    let mut args = args.to_vec();
    args.push(OsString::from("--help"));
    run_kiro(kiro, &args)
}

fn yolo(harness: ManagedHarness, native: &[&str], linked: Option<&str>) -> Vec<OsString> {
    let native = native.iter().map(OsString::from).collect();
    let mut plan = build_launch_plan(harness, None, native, linked).expect("launch plan");
    apply_yolo(harness, &mut plan.args);
    plan.args
}

/// Every shape a managed Kiro launch takes under `--yolo` is one the real
/// `kiro-cli` parses, with `--trust-all-tools` present in each.
#[test]
fn real_kiro_cli_accepts_every_yolo_launch_shape() {
    let Some(kiro) = kiro_cli() else {
        return;
    };
    let linked_v3 = Some("sess_3f6d1c2a-0000-4000-8000-000000000aaa");
    let linked_v2 = Some("3f6d1c2a-0000-4000-8000-000000000aaa");
    let launches = [
        yolo(ManagedHarness::Kiro, &[], None),
        yolo(ManagedHarness::Kiro, &[], linked_v2),
        yolo(ManagedHarness::Kiro, &["chat", "hi"], None),
        yolo(ManagedHarness::Kiro, &["--agent", "chat"], None),
        yolo(
            ManagedHarness::Kiro,
            &["--cloud", "--repo", "owner/name"],
            None,
        ),
        yolo(ManagedHarness::KiroV3, &[], None),
        yolo(ManagedHarness::KiroV3, &[], linked_v3),
        yolo(ManagedHarness::KiroV3, &["chat", "--v3", "hi"], None),
        yolo(
            ManagedHarness::KiroV3,
            &["-v", "chat", "--v3", "--tui"],
            None,
        ),
    ];
    for args in launches {
        assert!(
            args.iter().any(|arg| arg == "--trust-all-tools"),
            "{args:?}"
        );
        let output = parse_with_help(kiro, &args);
        assert!(
            output.status.success(),
            "kiro-cli rejected {args:?}:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// Why the placement matters: the root command rejects the flag, which is
/// where `--yolo` used to put it for a launch without `chat`.
#[test]
fn real_kiro_cli_rejects_trust_all_tools_outside_chat() {
    let Some(kiro) = kiro_cli() else {
        return;
    };
    for args in [&["--trust-all-tools"][..], &["--v3", "--trust-all-tools"]] {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        let output = parse_with_help(kiro, &args);
        assert!(
            !output.status.success(),
            "kiro-cli accepted {args:?} at the root; the chat placement would be moot"
        );
    }
}

/// A help or version request under `--yolo` still answers: those flags start
/// no session, and `chat` would reject the root's `--version` and
/// `--help-all`, so the wrapper adds nothing to them.
#[test]
fn real_kiro_cli_answers_help_and_version_under_yolo() {
    let Some(kiro) = kiro_cli() else {
        return;
    };
    for native in [&["--version"][..], &["-V"], &["--help-all"], &["--help"]] {
        for harness in [ManagedHarness::Kiro, ManagedHarness::KiroV3] {
            let args = yolo(harness, native, None);
            let output = run_kiro(kiro, &args);
            assert!(
                output.status.success(),
                "kiro-cli rejected {args:?}:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
