//! ai-jail detection and invocation assembly for `ai-memory run --yolo`.
//!
//! See `docs/design-yolo-safety-ai-jail.md`. Detection and argv assembly are
//! pure and dependency-injected so every OS branch and the exact argv shape
//! are unit-tested without a real sandbox or a real PATH.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Environment variable names forwarded into ai-jail with `--env NAME`, when
/// actually set in the current process environment. ai-jail clears the
/// environment by default and re-adds only what is explicitly named, so a
/// managed run's server/hook URL and the harness's own credentials would
/// otherwise be invisible inside the jail.
pub const FORWARDED_ENV_NAMES: &[&str] = &[
    "AI_MEMORY_SERVER_URL",
    "AI_MEMORY_HOOK_URL",
    "AI_MEMORY_DATA_DIR",
    "AI_MEMORY_AUTH_TOKEN",
    "CLAUDE_CONFIG_DIR",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "COPILOT_GITHUB_TOKEN",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
];

/// The ai-jail binary the `--yolo` offer may re-exec under, or `None` when
/// the offer must not be shown at all (docs/design-yolo-safety-ai-jail.md §2).
///
/// The offer is only made when accepting it can succeed: ai-jail does not
/// support Windows, and on Linux/macOS it cannot start without its sandbox
/// backend (`bwrap` / `sandbox-exec`). Accepting an offer that then fails
/// would cancel the already-prepared managed run for nothing. The returned
/// path is the one to exec, so the re-exec can never resolve a different —
/// or missing — binary than the one this check found. `lookup` is injected
/// so every OS branch is unit-tested without a real `PATH`.
#[must_use]
pub fn usable_ai_jail(os: JailOs, lookup: impl Fn(&str) -> Option<PathBuf>) -> Option<PathBuf> {
    let backend = match os {
        JailOs::Linux => "bwrap",
        JailOs::MacOs => "sandbox-exec",
        JailOs::Windows => return None,
    };
    lookup(backend)?;
    lookup("ai-jail")
}

/// [`usable_ai_jail`] for this host: the backend on `PATH`, and ai-jail on
/// `PATH` falling back to `~/.local/bin/ai-jail` (ai-jail's own documented
/// install location when that directory is not on `PATH`).
#[must_use]
pub fn usable_ai_jail_here() -> Option<PathBuf> {
    usable_ai_jail(current_jail_os(), |name| match find_on_path(name) {
        Some(path) => Some(path),
        None if name == "ai-jail" => home_local_bin_ai_jail(),
        None => None,
    })
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable_file(candidate))
}

fn home_local_bin_ai_jail() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let candidate = PathBuf::from(home)
        .join(".local")
        .join("bin")
        .join("ai-jail");
    is_executable_file(&candidate).then_some(candidate)
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The operating system whose ai-jail "already inside" signal applies.
/// Taken explicitly (rather than read from `cfg!`) so [`inside_ai_jail`]'s
/// branches are all reachable from a single-platform test run; the real
/// caller uses [`current_jail_os`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JailOs {
    /// Linux: ai-jail (bwrap) always sets the UTS hostname to `ai-sandbox`.
    Linux,
    /// macOS: ai-jail (seatbelt) has no UTS namespace, so it forces `PS1` to
    /// begin with `(jail) ` instead.
    MacOs,
    /// Windows: ai-jail is unsupported; never reports as jailed.
    Windows,
}

/// The host's actual OS, for the real (non-test) detection path.
#[must_use]
pub const fn current_jail_os() -> JailOs {
    if cfg!(target_os = "linux") {
        JailOs::Linux
    } else if cfg!(target_os = "macos") {
        JailOs::MacOs
    } else {
        JailOs::Windows
    }
}

/// Injected signals [`inside_ai_jail`] reads instead of the real process
/// environment, so detection is testable without a sandbox.
#[derive(Debug, Clone, Default)]
pub struct JailEnv {
    /// The current UTS hostname (Linux signal), e.g. from
    /// `/proc/sys/kernel/hostname`.
    pub hostname: Option<String>,
    /// The current `PS1` value (macOS signal).
    pub ps1: Option<String>,
}

/// Whether the process is already running inside ai-jail, per
/// `docs/design-yolo-safety-ai-jail.md` §3.
///
/// Fails open to the safe side: an unrecognized or missing signal returns
/// `false` (not jailed), so the yolo warning is shown rather than silently
/// skipped. A false positive is not plausible for either signal by
/// construction (ai-memory execs directly, not through an interactive
/// shell); a false negative just repeats the warning inside a real jail,
/// which is safe.
#[must_use]
pub fn inside_ai_jail(env: &JailEnv, os: JailOs) -> bool {
    match os {
        JailOs::Linux => env.hostname.as_deref() == Some("ai-sandbox"),
        JailOs::MacOs => env
            .ps1
            .as_deref()
            .is_some_and(|ps1| ps1.starts_with("(jail) ")),
        JailOs::Windows => false,
    }
}

/// Real "already inside ai-jail" check: reads the Linux hostname file and the
/// process's own `PS1`, then applies [`inside_ai_jail`] for [`current_jail_os`].
#[must_use]
pub fn inside_ai_jail_here() -> bool {
    let env = JailEnv {
        hostname: read_linux_hostname(),
        ps1: std::env::var("PS1").ok(),
    };
    inside_ai_jail(&env, current_jail_os())
}

fn read_linux_hostname() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|value| value.trim().to_string())
}

/// Build the argument vector for `ai-jail` (excluding the `ai-jail` program
/// name itself): `--network`, an optional bare `--agent-state` toggle, one
/// `--env NAME` per already-filtered present name, a `--` separator, then the
/// wrapped executable and its forwarded arguments in order.
///
/// The `--` is required, not cosmetic. ai-jail refuses one of its own flags
/// appearing after the command (it cannot tell whether
/// `ai-jail cmd --network` means the sandbox or the child), and `ai-memory
/// run` shares flag names with ai-jail — a forwarded `run claude --env
/// GH_TOKEN=…` was rejected outright. After `--`, ai-jail passes everything
/// to the wrapped command verbatim.
///
/// `--agent-state` is a boolean toggle in ai-jail (`--agent-state` /
/// `--no-agent-state`), not a valued flag — it persists the harness's own
/// credential state across ai-jail's otherwise-ephemeral private home; ai-jail
/// derives the per-harness state location itself from the wrapped
/// `ai-memory run <harness>` it parses. `env_names_present` is caller-filtered
/// (only names actually set in the current environment), keeping this function
/// pure and independent of the real process environment.
#[must_use]
pub fn build_ai_jail_invocation(
    exe: &Path,
    forwarded_args: &[OsString],
    env_names_present: &[&str],
    agent_state: bool,
) -> Vec<OsString> {
    let mut argv = vec![OsString::from("--network")];
    if agent_state {
        argv.push(OsString::from("--agent-state"));
    }
    for name in env_names_present {
        argv.push(OsString::from("--env"));
        argv.push(OsString::from(*name));
    }
    argv.push(OsString::from("--"));
    argv.push(exe.as_os_str().to_os_string());
    argv.extend(forwarded_args.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn usable_ai_jail_returns_the_resolved_binary_with_its_backend() {
        let found = |names: &'static [&'static str]| {
            move |name: &str| {
                names
                    .contains(&name)
                    .then(|| PathBuf::from(format!("/opt/bin/{name}")))
            }
        };
        assert_eq!(
            usable_ai_jail(JailOs::Linux, found(&["ai-jail", "bwrap"])),
            Some(PathBuf::from("/opt/bin/ai-jail"))
        );
        assert_eq!(
            usable_ai_jail(JailOs::MacOs, found(&["ai-jail", "sandbox-exec"])),
            Some(PathBuf::from("/opt/bin/ai-jail"))
        );
    }

    #[test]
    fn usable_ai_jail_is_none_when_ai_jail_is_missing() {
        assert_eq!(
            usable_ai_jail(JailOs::Linux, |name| {
                (name == "bwrap").then(|| PathBuf::from("/usr/bin/bwrap"))
            }),
            None
        );
    }

    /// ai-jail present but its sandbox backend absent: accepting the offer
    /// would cancel the prepared run and then fail, so it is not offered.
    #[test]
    fn usable_ai_jail_is_none_without_the_os_sandbox_backend() {
        let only_ai_jail =
            |name: &str| (name == "ai-jail").then(|| PathBuf::from("/usr/bin/ai-jail"));
        assert_eq!(usable_ai_jail(JailOs::Linux, only_ai_jail), None);
        assert_eq!(usable_ai_jail(JailOs::MacOs, only_ai_jail), None);
        // The other OS's backend does not count.
        let linux_backend_on_macos = |name: &str| {
            matches!(name, "ai-jail" | "bwrap").then(|| PathBuf::from(format!("/usr/bin/{name}")))
        };
        assert_eq!(usable_ai_jail(JailOs::MacOs, linux_backend_on_macos), None);
    }

    /// ai-jail is unsupported on Windows: even a file named `ai-jail` on PATH
    /// (a Git-Bash or WSL shim) must not produce the offer, and the lookup is
    /// never consulted.
    #[test]
    fn usable_ai_jail_is_never_offered_on_windows() {
        assert_eq!(
            usable_ai_jail(JailOs::Windows, |name| {
                panic!("Windows must not look up {name}")
            }),
            None
        );
    }

    /// The returned path is the exec target, so a binary found only through
    /// the `~/.local/bin` fallback is exec'd from there rather than re-resolved
    /// through `PATH` (where it would not be found).
    #[test]
    fn usable_ai_jail_returns_the_exact_lookup_path_to_exec() {
        let fallback = PathBuf::from("/home/dev/.local/bin/ai-jail");
        let expected = fallback.clone();
        let found = usable_ai_jail(JailOs::Linux, move |name| match name {
            "bwrap" => Some(PathBuf::from("/usr/bin/bwrap")),
            "ai-jail" => Some(fallback.clone()),
            _ => None,
        });
        assert_eq!(found, Some(expected));
    }

    #[test]
    fn inside_ai_jail_linux_matches_sandbox_hostname() {
        let jailed = JailEnv {
            hostname: Some("ai-sandbox".to_string()),
            ps1: None,
        };
        assert!(inside_ai_jail(&jailed, JailOs::Linux));

        let not_jailed = JailEnv {
            hostname: Some("dev-box".to_string()),
            ps1: None,
        };
        assert!(!inside_ai_jail(&not_jailed, JailOs::Linux));

        let unknown = JailEnv::default();
        assert!(!inside_ai_jail(&unknown, JailOs::Linux));
    }

    #[test]
    fn inside_ai_jail_macos_matches_ps1_prefix() {
        let jailed = JailEnv {
            hostname: None,
            ps1: Some("(jail) user@host $ ".to_string()),
        };
        assert!(inside_ai_jail(&jailed, JailOs::MacOs));

        let not_jailed = JailEnv {
            hostname: None,
            ps1: Some("user@host $ ".to_string()),
        };
        assert!(!inside_ai_jail(&not_jailed, JailOs::MacOs));

        let unknown = JailEnv::default();
        assert!(!inside_ai_jail(&unknown, JailOs::MacOs));
    }

    #[test]
    fn inside_ai_jail_windows_always_false() {
        let env = JailEnv {
            hostname: Some("ai-sandbox".to_string()),
            ps1: Some("(jail) ".to_string()),
        };
        assert!(!inside_ai_jail(&env, JailOs::Windows));
    }

    #[test]
    fn build_ai_jail_invocation_assembles_network_env_and_argv_in_order() {
        let exe = Path::new("/usr/local/bin/ai-memory");
        let forwarded = vec![
            OsString::from("run"),
            OsString::from("claude"),
            OsString::from("--yolo"),
        ];
        let present = ["AI_MEMORY_SERVER_URL", "ANTHROPIC_API_KEY"];
        let argv = build_ai_jail_invocation(exe, &forwarded, &present, true);
        assert_eq!(
            strings(&argv),
            [
                "--network",
                "--agent-state",
                "--env",
                "AI_MEMORY_SERVER_URL",
                "--env",
                "ANTHROPIC_API_KEY",
                "--",
                "/usr/local/bin/ai-memory",
                "run",
                "claude",
                "--yolo",
            ]
        );
    }

    #[test]
    fn build_ai_jail_invocation_omits_agent_state_when_none() {
        let exe = Path::new("/usr/local/bin/ai-memory");
        let argv = build_ai_jail_invocation(exe, &[], &[], false);
        assert_eq!(
            strings(&argv),
            ["--network", "--", "/usr/local/bin/ai-memory"]
        );
    }

    #[test]
    fn build_ai_jail_invocation_only_forwards_present_env_names() {
        let exe = Path::new("/bin/ai-memory");
        let argv = build_ai_jail_invocation(exe, &[], &["CLAUDE_CONFIG_DIR"], false);
        assert_eq!(
            strings(&argv),
            [
                "--network",
                "--env",
                "CLAUDE_CONFIG_DIR",
                "--",
                "/bin/ai-memory"
            ]
        );
    }

    /// Regression: forwarded `run` flags that share a name with ai-jail's own
    /// (`--env`, `--network`) must land after the `--` separator, where ai-jail
    /// passes them to the wrapped command instead of rejecting them.
    #[test]
    fn build_ai_jail_invocation_places_colliding_child_flags_after_separator() {
        let exe = Path::new("/bin/ai-memory");
        let forwarded = [
            "run",
            "claude",
            "--yolo",
            "--env",
            "GH_TOKEN=placeholder",
            "--network",
        ]
        .map(OsString::from);
        let argv = strings(&build_ai_jail_invocation(exe, &forwarded, &[], true));
        let separator = argv
            .iter()
            .position(|arg| arg == "--")
            .expect("separator present");
        assert_eq!(argv[separator + 1], "/bin/ai-memory");
        assert_eq!(
            &argv[separator + 2..],
            [
                "run",
                "claude",
                "--yolo",
                "--env",
                "GH_TOKEN=placeholder",
                "--network"
            ]
        );
        // Before the separator, only ai-memory's own sandbox flags appear.
        assert_eq!(&argv[..separator], ["--network", "--agent-state"]);
    }
}
