//! Native command planning without filtering harness arguments.

use std::ffi::OsString;
use std::path::PathBuf;

use ai_memory_core::AgentKind;
use anyhow::Result;
use uuid::Uuid;

/// Harnesses with native-session and transcript adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedHarness {
    /// Anthropic Claude Code.
    Claude,
    /// OpenAI Codex CLI.
    Codex,
    /// OpenCode.
    OpenCode,
    /// Pi coding agent.
    Pi,
    /// Charmbracelet Crush.
    Crush,
    /// Oh My Pi.
    Omp,
    /// Moonshot AI Kimi Code.
    Kimi,
}

impl ManagedHarness {
    /// Parse the user-facing command name.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "claude" | "claude-code" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" | "open-code" => Some(Self::OpenCode),
            "pi" => Some(Self::Pi),
            "crush" => Some(Self::Crush),
            "omp" | "oh-my-pi" => Some(Self::Omp),
            "kimi" | "kimi-code" | "kimi-cli" => Some(Self::Kimi),
            _ => None,
        }
    }

    /// Core agent kind used on the wire and in storage.
    #[must_use]
    pub const fn agent_kind(self) -> AgentKind {
        match self {
            Self::Claude => AgentKind::ClaudeCode,
            Self::Codex => AgentKind::Codex,
            Self::OpenCode => AgentKind::OpenCode,
            Self::Pi => AgentKind::Pi,
            Self::Crush => AgentKind::Crush,
            Self::Omp => AgentKind::Omp,
            Self::Kimi => AgentKind::KimiCode,
        }
    }

    /// Default executable resolved through `PATH`.
    #[must_use]
    pub const fn executable(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
            Self::Crush => "crush",
            Self::Omp => "omp",
            Self::Kimi => "kimi",
        }
    }

    /// Stable user-facing name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
            Self::Crush => "crush",
            Self::Omp => "omp",
            Self::Kimi => "kimi",
        }
    }
}

/// Whether the planned native invocation participates in session continuity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    /// Interactive or persisted native session.
    Session,
    /// Native utility/subcommand or explicitly ephemeral invocation. Arguments
    /// are still passed through and repository state is still checkpointed.
    Passthrough,
}

/// Fully constructed native process invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// Executable name/path.
    pub program: OsString,
    /// Native argument vector. User arguments retain byte/order identity.
    pub args: Vec<OsString>,
    /// Session id known before launch (generated, linked, or explicit).
    pub expected_session_id: Option<String>,
    /// Native transcript root resolved from explicit arguments or environment.
    pub session_dir: Option<PathBuf>,
    /// Session-bearing versus utility invocation.
    pub mode: LaunchMode,
}

/// Build the transparent resume/create command for one harness.
///
/// User arguments are never validated or rewritten. Adapter-owned session
/// selectors are inserted only when the invocation is session-bearing and the
/// user did not provide an explicit native selector.
pub fn build_launch_plan(
    harness: ManagedHarness,
    executable: Option<OsString>,
    native_args: Vec<OsString>,
    linked_session_id: Option<&str>,
) -> Result<LaunchPlan> {
    let program = executable.unwrap_or_else(|| OsString::from(harness.executable()));
    let mut args = native_args;
    let session_dir = match harness {
        ManagedHarness::Pi | ManagedHarness::Omp => flag_path(&args, &["--session-dir"]),
        ManagedHarness::Crush => flag_path(&args, &["--data-dir", "-D"]),
        _ => None,
    }
    .or_else(|| environment_session_dir(harness));
    let mut expected = explicit_session_id(harness, &args);
    let mode = launch_mode(harness, &args);
    if mode == LaunchMode::Session && !has_explicit_session_selector(harness, &args) {
        match harness {
            ManagedHarness::Claude => {
                let id = linked_session_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                if linked_session_id.is_some() {
                    args.extend([OsString::from("--resume"), OsString::from(&id)]);
                } else {
                    args.extend([OsString::from("--session-id"), OsString::from(&id)]);
                }
                expected = Some(id);
            }
            ManagedHarness::Codex => {
                if let Some(id) = linked_session_id {
                    let noninteractive = first_arg_is(&args, "exec");
                    let mut resumed = if noninteractive {
                        vec![
                            OsString::from("exec"),
                            OsString::from("resume"),
                            OsString::from(id),
                        ]
                    } else {
                        vec![OsString::from("resume"), OsString::from(id)]
                    };
                    resumed.extend(args.into_iter().skip(usize::from(noninteractive)));
                    args = resumed;
                    expected = Some(id.to_string());
                }
            }
            ManagedHarness::OpenCode => {
                if let Some(id) = linked_session_id {
                    if first_arg_is(&args, "run") {
                        args.insert(1, OsString::from(id));
                        args.insert(1, OsString::from("--session"));
                    } else {
                        args.insert(0, OsString::from(id));
                        args.insert(0, OsString::from("--session"));
                    }
                    expected = Some(id.to_string());
                }
            }
            ManagedHarness::Pi => {
                let id = linked_session_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                let selector = if linked_session_id.is_some() {
                    "--session"
                } else {
                    "--session-id"
                };
                args.extend([OsString::from(selector), OsString::from(&id)]);
                expected = Some(id);
            }
            ManagedHarness::Crush => {
                if let Some(id) = linked_session_id {
                    args.insert(0, OsString::from(id));
                    args.insert(0, OsString::from("--session"));
                    expected = Some(id.to_string());
                }
            }
            ManagedHarness::Omp => {
                if let Some(id) = linked_session_id {
                    args.push(OsString::from(format!("--resume={id}")));
                    expected = Some(id.to_string());
                }
            }
            ManagedHarness::Kimi => {
                // Kimi accepts no caller-chosen id for a fresh session, so
                // only a linked resume injects a selector. `--session <id>`
                // goes at the end: it is a commander option (position-free)
                // and user arguments are never reordered. The fresh session is
                // linked by the user-prompt hook or discovered post-exit.
                if let Some(id) = linked_session_id {
                    args.extend([OsString::from("--session"), OsString::from(id)]);
                    expected = Some(id.to_string());
                }
            }
        }
    }

    Ok(LaunchPlan {
        program,
        args,
        expected_session_id: expected,
        session_dir,
        mode,
    })
}

/// Apply the wrapper-owned dangerous-mode flag using native harness syntax.
/// Harnesses that already execute tools without a permission gate need no
/// extra argument.
pub fn apply_yolo(harness: ManagedHarness, args: &mut Vec<OsString>) {
    let flag = match harness {
        ManagedHarness::Claude => Some("--dangerously-skip-permissions"),
        ManagedHarness::Codex => Some("--dangerously-bypass-approvals-and-sandbox"),
        ManagedHarness::OpenCode => Some("--auto"),
        ManagedHarness::Pi => Some("--approve"),
        ManagedHarness::Crush => Some("--yolo"),
        ManagedHarness::Omp => None,
        ManagedHarness::Kimi => Some("--yolo"),
    };
    if let Some(flag) = flag {
        // Kimi's `--yolo` has hidden aliases (`--yes`, `--auto-approve`) and
        // conflicts with the distinct `--auto` mode, so any of those native
        // spellings already satisfies the wrapper's dangerous-mode request.
        let present: &[&str] = match harness {
            ManagedHarness::Kimi => &["--yolo", "-y", "--yes", "--auto-approve", "--auto"],
            _ => &[flag],
        };
        if !has_flag(args, present) {
            args.push(OsString::from(flag));
        }
    }
}

/// Whether a native invocation may use ai-memory's one-time adoption prompt.
/// Explicit selectors and utility/ephemeral invocations always pass through.
#[must_use]
pub fn allows_native_session_adoption(harness: ManagedHarness, native_args: &[OsString]) -> bool {
    launch_mode(harness, native_args) == LaunchMode::Session
        && !has_explicit_session_selector(harness, native_args)
        && !noninteractive_invocation(harness, native_args)
}

fn noninteractive_invocation(harness: ManagedHarness, args: &[OsString]) -> bool {
    match harness {
        ManagedHarness::Claude => has_flag(args, &["--print", "-p"]),
        ManagedHarness::Codex => first_arg_is(args, "exec"),
        ManagedHarness::OpenCode => first_arg_is(args, "run"),
        ManagedHarness::Crush => first_arg_is(args, "run"),
        ManagedHarness::Pi | ManagedHarness::Omp => has_flag(args, &["--print", "-p"]),
        ManagedHarness::Kimi => has_flag(args, &["--prompt", "-p"]),
    }
}

fn launch_mode(harness: ManagedHarness, args: &[OsString]) -> LaunchMode {
    if has_flag(args, &["--help", "-h", "--version", "-v"])
        || has_flag(args, &["--no-session", "--no-session-persistence"])
    {
        return LaunchMode::Passthrough;
    }
    if harness == ManagedHarness::Codex
        && first_arg_is(args, "exec")
        && args
            .get(1)
            .and_then(|arg| arg.to_str())
            .is_some_and(|command| matches!(command, "review" | "help"))
    {
        return LaunchMode::Passthrough;
    }
    let utility = match harness {
        ManagedHarness::Claude => [
            "agents",
            "auth",
            "auto-mode",
            "doctor",
            "install",
            "mcp",
            "plugin",
            "plugins",
            "project",
            "setup-token",
            "ultrareview",
            "update",
            "upgrade",
        ]
        .as_slice(),
        ManagedHarness::Codex => [
            "review",
            "login",
            "logout",
            "mcp",
            "plugin",
            "mcp-server",
            "app-server",
            "remote-control",
            "completion",
            "update",
            "doctor",
            "sandbox",
            "debug",
            "apply",
            "archive",
            "delete",
            "unarchive",
            "cloud",
            "exec-server",
            "features",
            "help",
        ]
        .as_slice(),
        ManagedHarness::OpenCode => [
            "completion",
            "acp",
            "mcp",
            "attach",
            "debug",
            "providers",
            "agent",
            "upgrade",
            "uninstall",
            "serve",
            "web",
            "models",
            "stats",
            "export",
            "import",
            "github",
            "pr",
            "session",
            "plugin",
            "db",
        ]
        .as_slice(),
        ManagedHarness::Pi => {
            ["install", "remove", "uninstall", "update", "list", "config"].as_slice()
        }
        ManagedHarness::Crush => [
            "completion",
            "dirs",
            "help",
            "login",
            "logout",
            "logs",
            "models",
            "projects",
            "server",
            "session",
            "stats",
            "update-providers",
        ]
        .as_slice(),
        ManagedHarness::Omp => [
            "acp",
            "agents",
            "auth-broker",
            "auth-gateway",
            "commit",
            "config",
            "grep",
            "grievances",
            "plugin",
            "read",
            "search",
            "setup",
            "shell",
            "ssh",
            "stats",
            "update",
            "worktree",
        ]
        .as_slice(),
        ManagedHarness::Kimi => [
            "export",
            "provider",
            "acp",
            "web",
            "server",
            "login",
            "doctor",
            "vis",
            "migrate",
            // `update` is an alias of `upgrade`; both must pass through.
            "upgrade",
            "update",
            "__plugin_run_node",
        ]
        .as_slice(),
    };
    let first = args.first().and_then(|arg| arg.to_str());
    if first.is_some_and(|value| utility.contains(&value)) {
        LaunchMode::Passthrough
    } else {
        LaunchMode::Session
    }
}

fn has_explicit_session_selector(harness: ManagedHarness, args: &[OsString]) -> bool {
    match harness {
        ManagedHarness::Claude => has_flag(
            args,
            &["--resume", "-r", "--continue", "-c", "--session-id"],
        ),
        ManagedHarness::Codex => {
            first_arg_is(args, "resume")
                || first_arg_is(args, "fork")
                || args.first().and_then(|arg| arg.to_str()) == Some("exec")
                    && args.get(1).and_then(|arg| arg.to_str()) == Some("resume")
        }
        ManagedHarness::OpenCode => {
            has_flag(args, &["--session", "-s", "--continue", "-c", "--fork"])
        }
        ManagedHarness::Pi => has_flag(
            args,
            &[
                "--session",
                "--session-id",
                "--continue",
                "-c",
                "--resume",
                "-r",
                "--fork",
            ],
        ),
        ManagedHarness::Crush => has_flag(args, &["--session", "-s", "--continue", "-C"]),
        ManagedHarness::Omp => has_flag(args, &["--resume", "-r", "--continue", "-c"]),
        // `--resume`/`-r` is a hidden alias of `--session`; `-C` is a hidden
        // alias of `--continue`. A bare `--session` opens the native picker,
        // which still counts as an explicit user choice.
        ManagedHarness::Kimi => has_flag(
            args,
            &[
                "--session",
                "-S",
                "--resume",
                "-r",
                "--continue",
                "-c",
                "-C",
            ],
        ),
    }
}

fn explicit_session_id(harness: ManagedHarness, args: &[OsString]) -> Option<String> {
    match harness {
        ManagedHarness::Claude => flag_value(args, &["--resume", "-r", "--session-id"]),
        ManagedHarness::Codex => {
            if first_arg_is(args, "exec")
                && args.get(1).and_then(|arg| arg.to_str()) == Some("resume")
            {
                args.get(2)
                    .and_then(|value| value.to_str())
                    .filter(|value| !value.starts_with('-'))
                    .map(str::to_owned)
            } else {
                positional_after_command(args, &["resume"])
            }
        }
        ManagedHarness::OpenCode => flag_value(args, &["--session", "-s"]),
        ManagedHarness::Pi => flag_value(args, &["--session", "--session-id"]),
        ManagedHarness::Crush => flag_value(args, &["--session", "-s"]),
        ManagedHarness::Omp => flag_value(args, &["--resume", "-r"]),
        // A bare `--session`/`--resume` opens the picker: `flag_value`
        // returns `None` when no value follows, as intended.
        ManagedHarness::Kimi => flag_value(args, &["--session", "-S", "--resume", "-r"]),
    }
}

fn first_arg_is(args: &[OsString], expected: &str) -> bool {
    args.first().and_then(|value| value.to_str()) == Some(expected)
}

fn has_flag(args: &[OsString], names: &[&str]) -> bool {
    args.iter().any(|arg| {
        let Some(value) = arg.to_str() else {
            return false;
        };
        names
            .iter()
            .any(|name| value == *name || value.starts_with(&format!("{name}=")))
    })
}

fn flag_value(args: &[OsString], names: &[&str]) -> Option<String> {
    for (index, arg) in args.iter().enumerate() {
        let value = arg.to_str()?;
        for name in names {
            if value == *name {
                return args
                    .get(index + 1)
                    .and_then(|next| next.to_str())
                    .filter(|next| !next.starts_with('-'))
                    .map(str::to_owned);
            }
            if let Some(found) = value.strip_prefix(&format!("{name}="))
                && !found.is_empty()
            {
                return Some(found.to_string());
            }
        }
    }
    None
}

fn flag_path(args: &[OsString], names: &[&str]) -> Option<PathBuf> {
    for (index, arg) in args.iter().enumerate() {
        if names.iter().any(|name| arg == *name) {
            return args.get(index + 1).map(PathBuf::from);
        }
        let Some(value) = arg.to_str() else {
            continue;
        };
        for name in names {
            if let Some(found) = value.strip_prefix(&format!("{name}="))
                && !found.is_empty()
            {
                return Some(PathBuf::from(found));
            }
        }
    }
    None
}

fn environment_session_dir(harness: ManagedHarness) -> Option<PathBuf> {
    environment_session_dir_with(harness, |name| std::env::var_os(name))
}

fn environment_session_dir_with(
    harness: ManagedHarness,
    get: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let value = |name| {
        get(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    match harness {
        ManagedHarness::Claude => value("CLAUDE_CONFIG_DIR").map(|dir| dir.join("projects")),
        ManagedHarness::Codex => value("CODEX_HOME").map(|dir| dir.join("sessions")),
        ManagedHarness::OpenCode => value("XDG_DATA_HOME").map(|dir| dir.join("opencode")),
        ManagedHarness::Pi => value("PI_CODING_AGENT_SESSION_DIR")
            .or_else(|| value("PI_CODING_AGENT_DIR").map(|dir| dir.join("sessions"))),
        ManagedHarness::Crush => None,
        ManagedHarness::Omp => value("PI_CODING_AGENT_DIR").map(|dir| dir.join("sessions")),
        // Sessions live under `<KIMI_CODE_HOME>/sessions/<bucket>/<id>/`.
        ManagedHarness::Kimi => value("KIMI_CODE_HOME").map(|dir| dir.join("sessions")),
    }
}

fn positional_after_command(args: &[OsString], commands: &[&str]) -> Option<String> {
    let command = args.first()?.to_str()?;
    if !commands.contains(&command) {
        return None;
    }
    args.get(1)
        .and_then(|value| value.to_str())
        .filter(|value| !value.starts_with('-'))
        .map(str::to_owned)
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
    fn claude_generates_then_resumes_native_session() {
        let fresh = build_launch_plan(ManagedHarness::Claude, None, vec![], None).unwrap();
        let id = fresh.expected_session_id.clone().unwrap();
        assert_eq!(strings(&fresh.args), ["--session-id", id.as_str()]);

        let resumed = build_launch_plan(
            ManagedHarness::Claude,
            None,
            vec![OsString::from("--model"), OsString::from("opus")],
            Some(&id),
        )
        .unwrap();
        assert_eq!(
            strings(&resumed.args),
            ["--model", "opus", "--resume", id.as_str()]
        );
    }

    #[test]
    fn codex_resume_preserves_all_user_arguments_in_order() {
        let native = vec![
            OsString::from("--yolo"),
            OsString::from("-m"),
            OsString::from("gpt-5"),
            OsString::from("continue here"),
        ];
        let plan =
            build_launch_plan(ManagedHarness::Codex, None, native, Some("codex-id")).unwrap();
        assert_eq!(
            strings(&plan.args),
            [
                "resume",
                "codex-id",
                "--yolo",
                "-m",
                "gpt-5",
                "continue here"
            ]
        );
    }

    #[test]
    fn codex_exec_resume_uses_native_noninteractive_subcommand() {
        let native = vec![
            OsString::from("exec"),
            OsString::from("--json"),
            OsString::from("continue here"),
        ];
        let plan =
            build_launch_plan(ManagedHarness::Codex, None, native, Some("codex-id")).unwrap();
        assert_eq!(
            strings(&plan.args),
            ["exec", "resume", "codex-id", "--json", "continue here"]
        );
        assert_eq!(plan.mode, LaunchMode::Session);
    }

    #[test]
    fn explicit_codex_exec_resume_wins() {
        let native = vec![
            OsString::from("exec"),
            OsString::from("resume"),
            OsString::from("chosen"),
            OsString::from("continue here"),
        ];
        let plan = build_launch_plan(ManagedHarness::Codex, None, native, Some("linked")).unwrap();
        assert_eq!(
            strings(&plan.args),
            ["exec", "resume", "chosen", "continue here"]
        );
        assert_eq!(plan.expected_session_id.as_deref(), Some("chosen"));
    }

    #[test]
    fn explicit_native_selector_wins() {
        let plan = build_launch_plan(
            ManagedHarness::OpenCode,
            None,
            vec![OsString::from("--session=chosen"), OsString::from("--auto")],
            Some("linked"),
        )
        .unwrap();
        assert_eq!(strings(&plan.args), ["--session=chosen", "--auto"]);
        assert_eq!(plan.expected_session_id.as_deref(), Some("chosen"));
    }

    #[test]
    fn adoption_is_only_allowed_for_session_launches_without_a_selector() {
        assert!(allows_native_session_adoption(
            ManagedHarness::Codex,
            &[OsString::from("--yolo")]
        ));
        assert!(allows_native_session_adoption(
            ManagedHarness::OpenCode,
            &[OsString::from("--auto")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::Codex,
            &[OsString::from("resume")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::Claude,
            &[OsString::from("--continue")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::Pi,
            &[OsString::from("--no-session")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::Codex,
            &[OsString::from("login")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::Codex,
            &[OsString::from("exec"), OsString::from("continue here")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::Claude,
            &[OsString::from("--print"), OsString::from("continue here")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::OpenCode,
            &[OsString::from("run"), OsString::from("continue here")]
        ));
    }

    #[test]
    fn opencode_resume_places_selector_after_run_subcommand() {
        let plan = build_launch_plan(
            ManagedHarness::OpenCode,
            None,
            vec![OsString::from("run"), OsString::from("continue here")],
            Some("open-code-id"),
        )
        .unwrap();
        assert_eq!(
            strings(&plan.args),
            ["run", "--session", "open-code-id", "continue here"]
        );
        assert_eq!(plan.expected_session_id.as_deref(), Some("open-code-id"));
    }

    #[test]
    fn pi_generates_then_resumes_native_session() {
        let fresh = build_launch_plan(
            ManagedHarness::Pi,
            None,
            vec![OsString::from("continue here")],
            None,
        )
        .unwrap();
        let id = fresh.expected_session_id.clone().unwrap();
        assert_eq!(
            strings(&fresh.args),
            ["continue here", "--session-id", id.as_str()]
        );

        let resumed = build_launch_plan(
            ManagedHarness::Pi,
            None,
            vec![OsString::from("continue here")],
            Some(&id),
        )
        .unwrap();
        assert_eq!(
            strings(&resumed.args),
            ["continue here", "--session", id.as_str()]
        );
    }

    #[test]
    fn crush_resumes_linked_session_and_observes_data_directory() {
        let plan = build_launch_plan(
            ManagedHarness::Crush,
            None,
            vec![
                OsString::from("--data-dir"),
                OsString::from("/tmp/crush-data"),
            ],
            Some("crush-id"),
        )
        .unwrap();
        assert_eq!(
            strings(&plan.args),
            ["--session", "crush-id", "--data-dir", "/tmp/crush-data"]
        );
        assert_eq!(plan.expected_session_id.as_deref(), Some("crush-id"));
        assert_eq!(
            plan.session_dir.as_deref(),
            Some(std::path::Path::new("/tmp/crush-data"))
        );
    }

    #[test]
    fn wrapper_yolo_uses_each_harness_native_flag_without_duplicates() {
        for (harness, expected) in [
            (
                ManagedHarness::Claude,
                Some("--dangerously-skip-permissions"),
            ),
            (
                ManagedHarness::Codex,
                Some("--dangerously-bypass-approvals-and-sandbox"),
            ),
            (ManagedHarness::OpenCode, Some("--auto")),
            (ManagedHarness::Pi, Some("--approve")),
            (ManagedHarness::Crush, Some("--yolo")),
            (ManagedHarness::Omp, None),
            (ManagedHarness::Kimi, Some("--yolo")),
        ] {
            let mut args = Vec::new();
            apply_yolo(harness, &mut args);
            apply_yolo(harness, &mut args);
            assert_eq!(
                strings(&args),
                expected.into_iter().collect::<Vec<_>>(),
                "{} yolo mapping",
                harness.as_str()
            );
        }
    }

    #[test]
    fn omp_resume_uses_equals_form_without_reordering_native_args() {
        let plan = build_launch_plan(
            ManagedHarness::Omp,
            None,
            vec![OsString::from("--yolo"), OsString::from("continue here")],
            Some("omp-id"),
        )
        .unwrap();
        assert_eq!(
            strings(&plan.args),
            ["--yolo", "continue here", "--resume=omp-id"]
        );
        assert_eq!(plan.expected_session_id.as_deref(), Some("omp-id"));
    }

    #[test]
    fn pi_family_session_directory_is_observed_without_changing_native_argv() {
        let pi_args = vec![
            OsString::from("--session-dir"),
            OsString::from("/tmp/pi sessions"),
            OsString::from("continue here"),
        ];
        let pi = build_launch_plan(ManagedHarness::Pi, None, pi_args.clone(), None).unwrap();
        assert_eq!(
            pi.session_dir.as_deref(),
            Some(std::path::Path::new("/tmp/pi sessions"))
        );
        assert_eq!(&pi.args[..pi_args.len()], pi_args);

        let omp = build_launch_plan(
            ManagedHarness::Omp,
            None,
            vec![OsString::from("--session-dir=/tmp/omp")],
            None,
        )
        .unwrap();
        assert_eq!(
            omp.session_dir.as_deref(),
            Some(std::path::Path::new("/tmp/omp"))
        );
    }

    #[test]
    fn native_store_environment_overrides_match_harness_layouts() {
        let get = |name: &str| match name {
            "CLAUDE_CONFIG_DIR" => Some(OsString::from("/stores/claude")),
            "CODEX_HOME" => Some(OsString::from("/stores/codex")),
            "XDG_DATA_HOME" => Some(OsString::from("/stores/xdg")),
            "PI_CODING_AGENT_DIR" => Some(OsString::from("/stores/pi-family")),
            _ => None,
        };
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Claude, get).as_deref(),
            Some(std::path::Path::new("/stores/claude/projects"))
        );
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Codex, get).as_deref(),
            Some(std::path::Path::new("/stores/codex/sessions"))
        );
        assert_eq!(
            environment_session_dir_with(ManagedHarness::OpenCode, get).as_deref(),
            Some(std::path::Path::new("/stores/xdg/opencode"))
        );
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Omp, get).as_deref(),
            Some(std::path::Path::new("/stores/pi-family/sessions"))
        );
    }

    #[test]
    fn utility_subcommands_are_passed_through_without_resume_flags() {
        let plan = build_launch_plan(
            ManagedHarness::Codex,
            None,
            vec![OsString::from("doctor")],
            Some("linked"),
        )
        .unwrap();
        assert_eq!(plan.mode, LaunchMode::Passthrough);
        assert_eq!(strings(&plan.args), ["doctor"]);
    }

    #[test]
    fn kimi_fresh_launch_injects_no_session_selector() {
        // Kimi rejects caller-chosen ids for new sessions, so a fresh launch
        // must leave argv untouched even though the harness is managed.
        let plan = build_launch_plan(
            ManagedHarness::Kimi,
            None,
            vec![OsString::from("continue here")],
            None,
        )
        .unwrap();
        assert_eq!(strings(&plan.args), ["continue here"]);
        assert_eq!(plan.expected_session_id, None);
        assert_eq!(plan.mode, LaunchMode::Session);
    }

    #[test]
    fn kimi_cli_name_is_an_alias_for_kimi_code() {
        for name in ["kimi", "kimi-code", "kimi-cli"] {
            assert_eq!(ManagedHarness::from_name(name), Some(ManagedHarness::Kimi));
        }
    }

    #[test]
    fn kimi_resume_appends_session_selector_after_user_arguments() {
        let plan = build_launch_plan(
            ManagedHarness::Kimi,
            None,
            vec![OsString::from("--model"), OsString::from("k2")],
            Some("session_abc"),
        )
        .unwrap();
        assert_eq!(
            strings(&plan.args),
            ["--model", "k2", "--session", "session_abc"]
        );
        assert_eq!(plan.expected_session_id.as_deref(), Some("session_abc"));
    }

    #[test]
    fn kimi_explicit_selector_always_wins_including_bare_picker() {
        for native in [
            vec![
                OsString::from("--session"),
                OsString::from("session_chosen"),
            ],
            vec![OsString::from("--resume=session_chosen")],
            vec![OsString::from("-c")],
            // Bare `--session` opens the native picker; it is still an
            // explicit user choice, so nothing may be injected and no id is
            // known up front.
            vec![OsString::from("--session")],
        ] {
            let plan = build_launch_plan(
                ManagedHarness::Kimi,
                None,
                native.clone(),
                Some("session_linked"),
            )
            .unwrap();
            assert_eq!(plan.args, native, "{native:?} must stay byte-identical");
            assert_ne!(plan.expected_session_id.as_deref(), Some("session_linked"));
        }
        let chosen = build_launch_plan(
            ManagedHarness::Kimi,
            None,
            vec![
                OsString::from("--session"),
                OsString::from("session_chosen"),
            ],
            Some("session_linked"),
        )
        .unwrap();
        assert_eq!(
            chosen.expected_session_id.as_deref(),
            Some("session_chosen")
        );
    }

    #[test]
    fn kimi_utility_subcommands_are_passed_through() {
        for utility in ["export", "doctor", "provider", "upgrade", "server"] {
            let plan = build_launch_plan(
                ManagedHarness::Kimi,
                None,
                vec![OsString::from(utility)],
                Some("session_linked"),
            )
            .unwrap();
            assert_eq!(plan.mode, LaunchMode::Passthrough, "{utility}");
            assert_eq!(strings(&plan.args), [utility]);
        }
    }

    #[test]
    fn kimi_noninteractive_prompt_blocks_adoption() {
        assert!(!allows_native_session_adoption(
            ManagedHarness::Kimi,
            &[OsString::from("-p"), OsString::from("summarize")]
        ));
        assert!(!allows_native_session_adoption(
            ManagedHarness::Kimi,
            &[OsString::from("--prompt"), OsString::from("summarize")]
        ));
        assert!(allows_native_session_adoption(
            ManagedHarness::Kimi,
            &[OsString::from("--model"), OsString::from("k2")]
        ));
    }

    #[test]
    fn kimi_yolo_respects_native_aliases_and_auto_conflict() {
        for already in ["--yolo", "-y", "--yes", "--auto-approve", "--auto"] {
            let mut args = vec![OsString::from(already)];
            apply_yolo(ManagedHarness::Kimi, &mut args);
            assert_eq!(strings(&args), [already], "{already} must not duplicate");
        }
    }

    #[test]
    fn kimi_home_environment_override_points_at_sessions_root() {
        let get =
            |name: &str| (name == "KIMI_CODE_HOME").then(|| OsString::from("/stores/kimi-code"));
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Kimi, get).as_deref(),
            Some(std::path::Path::new("/stores/kimi-code/sessions"))
        );
    }
}
