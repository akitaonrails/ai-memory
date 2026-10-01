//! Integration coverage for the `--yolo` ai-jail re-exec contract.
//!
//! Unit tests in `ai-memory-workstream::jail` prove the pure argv assembly and
//! per-OS detection. These tests assert the *cross-tool* contract: the argv
//! `build_ai_jail_invocation` produces is one the real `ai-jail` binary accepts
//! and forwards unchanged. The real-`ai-jail` tests skip cleanly when the
//! feature itself would not offer ai-jail on this host (not installed, no
//! sandbox backend, or Windows), mirroring the opt-in discipline of
//! `tests/e2e/handoff_smoke.sh`, so CI without a sandbox stays green.

use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

use ai_memory_workstream::{build_ai_jail_invocation, usable_ai_jail_here};

fn forwarded() -> Vec<OsString> {
    ["run", "claude", "--yolo"]
        .into_iter()
        .map(OsString::from)
        .collect()
}

/// ai-jail's parser (README "Positional command behavior is sacred") treats the
/// first positional token as the wrapped program and captures everything after
/// it verbatim. So every sandbox flag we emit must precede the wrapped exe, and
/// the token right after the leading flags must be the exe itself — never a
/// stray value. This is the exact shape a value-taking `--agent-state` would
/// have broken (it would have made ai-jail run the value as the command).
#[test]
fn invocation_puts_all_sandbox_flags_before_the_wrapped_exe() {
    let exe = Path::new("/usr/local/bin/ai-memory");
    let argv = build_ai_jail_invocation(
        exe,
        &forwarded(),
        &["AI_MEMORY_SERVER_URL", "ANTHROPIC_API_KEY"],
        true,
    );
    let strs: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    // The wrapped exe is the first positional; everything before it is a known
    // sandbox flag (or the value of `--env`), and everything from it on is the
    // wrapped command.
    let exe_pos = strs
        .iter()
        .position(|s| s == "/usr/local/bin/ai-memory")
        .expect("wrapped exe present in argv");
    assert_eq!(
        &strs[exe_pos..],
        &["/usr/local/bin/ai-memory", "run", "claude", "--yolo"],
        "the wrapped command must be forwarded verbatim, right after the exe"
    );
    // The `--` separator sits immediately before the exe, so no forwarded
    // flag can ever be parsed as one of ai-jail's own.
    assert_eq!(strs[exe_pos - 1], "--", "`--` must precede the wrapped exe");

    // `--agent-state` is a bare toggle: it must be followed by another flag or
    // the exe, never by a value ai-jail would misread as the command.
    let agent_state = strs
        .iter()
        .position(|s| s == "--agent-state")
        .expect("agent-state flag");
    let after = &strs[agent_state + 1];
    assert!(
        after.starts_with("--"),
        "--agent-state must be a bare toggle, but is followed by {after:?}"
    );
}

/// `--env` is emitted only for names the caller marked present, one flag per
/// name, and never with an inline value (ai-jail forwards the host value for a
/// bare `--env NAME`).
#[test]
fn invocation_emits_one_bare_env_flag_per_present_name() {
    let exe = Path::new("/bin/ai-memory");
    let argv = build_ai_jail_invocation(exe, &forwarded(), &["CLAUDE_CONFIG_DIR"], false);
    let strs: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let env_flags = strs.iter().filter(|s| *s == "--env").count();
    assert_eq!(env_flags, 1);
    let idx = strs.iter().position(|s| s == "--env").unwrap();
    assert_eq!(strs[idx + 1], "CLAUDE_CONFIG_DIR");
    assert!(
        !strs[idx + 1].contains('='),
        "forward the name, not name=value"
    );
    // agent_state=false → no toggle present.
    assert!(!strs.iter().any(|s| s == "--agent-state"));
}

/// Run the real `ai-jail --dry-run` (prints the sandbox command without
/// executing it) over the argv built for `forwarded`, wrapping this test binary
/// so any failure is about the argv shape rather than an unresolvable command.
/// `None` ⇒ the feature would not offer ai-jail on this host, so skip.
fn real_dry_run(forwarded: &[OsString]) -> Option<(std::process::Output, String, String)> {
    let Some(ai_jail) = usable_ai_jail_here() else {
        eprintln!("skipping: ai-jail is not usable here (absent, no sandbox backend, or Windows)");
        return None;
    };
    let exe = std::env::current_exe().expect("test binary path");
    let argv = build_ai_jail_invocation(&exe, forwarded, &["AI_MEMORY_SERVER_URL"], true);
    let output = Command::new(&ai_jail)
        .arg("--dry-run")
        .args(&argv)
        .output()
        .expect("run ai-jail --dry-run");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Some((output, combined, exe.to_string_lossy().into_owned()))
}

/// The real integration check: the argv we build is accepted by the installed
/// `ai-jail`, and the wrapped `ai-memory run claude --yolo` survives verbatim.
/// A malformed invocation — e.g. a value-taking `--agent-state` swallowing the
/// exe — fails here.
#[test]
fn real_ai_jail_dry_run_accepts_and_forwards_the_invocation() {
    let Some((output, combined, exe)) = real_dry_run(&forwarded()) else {
        return;
    };
    assert!(
        output.status.success(),
        "ai-jail --dry-run rejected the invocation:\n{combined}"
    );
    for token in ["run", "claude", "--yolo"] {
        assert!(
            combined.contains(token),
            "dry-run plan is missing the wrapped token {token:?}; plan was:\n{combined}"
        );
    }
    assert!(
        combined.contains(&exe),
        "dry-run plan should name the wrapped ai-memory exe; plan was:\n{combined}"
    );
}

/// First ai-jail release whose post-command flag guard honors `--`. Older ones
/// reject a child `--env` even after the separator (their error text still
/// says "use --"), so the cross-tool regression below can only bite from here.
const AI_JAIL_HONORS_SEPARATOR: (u32, u32, u32) = (2, 4, 2);

fn ai_jail_version(ai_jail: &Path) -> Option<(u32, u32, u32)> {
    let output = Command::new(ai_jail).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace().nth(1)?.split('.');
    let mut next = || parts.next()?.parse::<u32>().ok();
    Some((next()?, next()?, next()?))
}

/// Regression for the rejected `ai-memory run claude --yolo --env GH_TOKEN=…`:
/// ai-jail refuses one of its own flags after the command unless a `--`
/// separates them, so a forwarded `--env`/`--network` must reach the child
/// intact instead of aborting the launch. The `--` placement itself is pinned
/// unconditionally by the `ai-memory-workstream` unit tests.
#[test]
fn real_ai_jail_dry_run_forwards_child_flags_that_collide_with_its_own() {
    if let Some(ai_jail) = usable_ai_jail_here()
        && ai_jail_version(&ai_jail).is_none_or(|version| version < AI_JAIL_HONORS_SEPARATOR)
    {
        eprintln!(
            "skipping: this ai-jail predates {AI_JAIL_HONORS_SEPARATOR:?} and ignores `--` in its post-command flag guard"
        );
        return;
    }
    let forwarded: Vec<OsString> = [
        "run",
        "claude",
        "--yolo",
        "--env",
        "GH_TOKEN=placeholder",
        "--network",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    let Some((output, combined, _)) = real_dry_run(&forwarded) else {
        return;
    };
    assert!(
        output.status.success(),
        "ai-jail rejected a forwarded child flag that shares its name:\n{combined}"
    );
    assert!(
        combined.contains("GH_TOKEN=placeholder"),
        "the child's --env value must be forwarded verbatim; plan was:\n{combined}"
    );
}
