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
    /// OpenCode 2.0 beta (`opencode2` binary, side-by-side with v1).
    /// Shares v1's config dir, session store, and agent kind; only the
    /// launched executable differs.
    OpenCode2,
    /// Pi coding agent.
    Pi,
    /// Charmbracelet Crush.
    Crush,
    /// Oh My Pi.
    Omp,
    /// Moonshot AI Kimi Code.
    Kimi,
    /// Command Code CLI.
    CommandCode,
    /// Amazon Kiro CLI (v2 engine).
    Kiro,
    /// Amazon Kiro CLI (v3 engine).
    KiroV3,
    /// Grok Build CLI (xAI).
    Grok,
    /// Google Antigravity CLI (`agy`).
    Antigravity,
}

impl ManagedHarness {
    /// Parse the user-facing command name.
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "claude" | "claude-code" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "opencode" | "open-code" => Some(Self::OpenCode),
            "opencode2" | "opencode-v2" | "open-code2" => Some(Self::OpenCode2),
            "pi" => Some(Self::Pi),
            "crush" => Some(Self::Crush),
            "omp" | "oh-my-pi" => Some(Self::Omp),
            "kimi" | "kimi-code" | "kimi-cli" => Some(Self::Kimi),
            "command-code" | "commandcode" | "cmdc" | "cmd" => Some(Self::CommandCode),
            "kiro" | "kiro-cli" => Some(Self::Kiro),
            "grok" | "grok-build" => Some(Self::Grok),
            "antigravity" | "antigravity-cli" | "agy" => Some(Self::Antigravity),
            _ => None,
        }
    }

    /// Core agent kind used on the wire and in storage.
    #[must_use]
    pub const fn agent_kind(self) -> AgentKind {
        match self {
            Self::Claude => AgentKind::ClaudeCode,
            Self::Codex => AgentKind::Codex,
            Self::OpenCode | Self::OpenCode2 => AgentKind::OpenCode,
            Self::Pi => AgentKind::Pi,
            Self::Crush => AgentKind::Crush,
            Self::Omp => AgentKind::Omp,
            Self::Kimi => AgentKind::KimiCode,
            Self::CommandCode => AgentKind::CommandCode,
            Self::Kiro | Self::KiroV3 => AgentKind::KiroCli,
            Self::Grok => AgentKind::Grok,
            Self::Antigravity => AgentKind::AntigravityCli,
        }
    }

    /// Default executable resolved through `PATH`.
    #[must_use]
    pub const fn executable(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::OpenCode2 => "opencode2",
            Self::Pi => "pi",
            Self::Crush => "crush",
            Self::Omp => "omp",
            Self::Kimi => "kimi",
            Self::CommandCode => {
                if cfg!(windows) {
                    "cmdc"
                } else {
                    "command-code"
                }
            }
            Self::Kiro | Self::KiroV3 => "kiro-cli",
            Self::Grok => "grok",
            Self::Antigravity => "agy",
        }
    }

    /// Stable user-facing name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
            Self::OpenCode2 => "opencode2",
            Self::Pi => "pi",
            Self::Crush => "crush",
            Self::Omp => "omp",
            Self::Kimi => "kimi",
            Self::CommandCode => "command-code",
            Self::Kiro => "kiro",
            Self::KiroV3 => "kiro-v3",
            Self::Grok => "grok",
            Self::Antigravity => "antigravity",
        }
    }
}

/// Whether a Kiro CLI invocation targets an agent engine other than the
/// default v2 engine — `--v3`, `--mode` (a v3-only option), or an
/// `--agent-engine` value that is not `v2` (the `chat` subcommand's
/// engine selector, verified on kiro-cli 2.16.2).
///
/// Kiro v3 sessions live in a separate id space and cannot be resumed by
/// the v2 engine (nor vice versa). Unknown non-v2 engines pass through rather
/// than being assigned to a known adapter.
#[must_use]
pub fn kiro_selects_non_default_engine(args: &[OsString]) -> bool {
    if has_flag(args, &["--v3", "--mode"]) {
        return true;
    }
    if !has_flag(args, &["--agent-engine"]) {
        return false;
    }
    flag_value(args, &["--agent-engine"]).as_deref() != Some("v2")
}

/// Whether Kiro CLI arguments explicitly select the v3 engine.
///
/// `--mode` is v3-only. An unknown `--agent-engine` value is not treated as
/// v3: callers leave such invocations in passthrough mode instead of guessing
/// which incompatible session store they use.
#[must_use]
pub fn kiro_selects_v3_engine(args: &[OsString]) -> bool {
    has_flag(args, &["--v3", "--mode"])
        || flag_value(args, &["--agent-engine"]).as_deref() == Some("v3")
}

/// Whether Kiro CLI arguments explicitly select the v2 engine.
#[must_use]
pub fn kiro_selects_v2_engine(args: &[OsString]) -> bool {
    flag_value(args, &["--agent-engine"]).as_deref() == Some("v2")
}

/// Exact Kiro session id supplied through `--resume-id`, when present.
#[must_use]
pub fn kiro_explicit_session_id(args: &[OsString]) -> Option<String> {
    flag_value(args, &["--resume-id"])
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
    if mode == LaunchMode::Session
        && harness == ManagedHarness::KiroV3
        && !kiro_selects_v3_engine(&args)
    {
        args.insert(0, OsString::from("--v3"));
    }
    if mode == LaunchMode::Session && !has_native_session_selector(harness, &args) {
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
            ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
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
            ManagedHarness::CommandCode => {
                // Command Code assigns UUIDs to fresh sessions. Use its exact
                // session selector for a linked resume; a fresh session is
                // discovered from the versioned transcript header after exit.
                if let Some(id) = linked_session_id {
                    args.extend([OsString::from("--session"), OsString::from(id)]);
                    expected = Some(id.to_string());
                }
            }
            ManagedHarness::Kiro | ManagedHarness::KiroV3 => {
                // Both engines assign ids to fresh sessions. A linked session
                // can be selected exactly after its engine-specific store has
                // been validated; a fresh one is discovered after exit.
                if let Some(id) = linked_session_id {
                    args.extend([OsString::from("--resume-id"), OsString::from(id)]);
                    expected = Some(id.to_string());
                }
            }
            ManagedHarness::Grok => {
                let id = linked_session_id
                    .map(str::to_owned)
                    .unwrap_or_else(|| Uuid::new_v4().to_string());
                let selector = if linked_session_id.is_some() {
                    "--resume"
                } else {
                    "--session-id"
                };
                args.extend([OsString::from(selector), OsString::from(&id)]);
                expected = Some(id);
            }
            ManagedHarness::Antigravity => {
                // `agy` accepts no caller-chosen id for a fresh conversation,
                // so only a linked resume injects a selector. The fresh
                // conversation is linked by the hooks or discovered from the
                // conversation store after exit.
                if let Some(id) = linked_session_id {
                    args.extend([OsString::from("--conversation"), OsString::from(id)]);
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
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => Some("--auto"),
        ManagedHarness::Pi => Some("--approve"),
        ManagedHarness::Crush => Some("--yolo"),
        ManagedHarness::Omp => None,
        ManagedHarness::Kimi => Some("--yolo"),
        ManagedHarness::CommandCode => Some("--yolo"),
        ManagedHarness::Kiro => {
            if kiro_selects_non_default_engine(args) {
                None
            } else {
                Some("--trust-all-tools")
            }
        }
        ManagedHarness::KiroV3 => None,
        ManagedHarness::Grok => Some("--yolo"),
        ManagedHarness::Antigravity => Some("--dangerously-skip-permissions"),
    };
    if let Some(flag) = flag {
        // Kimi's `--yolo` has hidden aliases (`--yes`, `--auto-approve`) and
        // conflicts with the distinct `--auto` mode, so any of those native
        // spellings already satisfies the wrapper's dangerous-mode request.
        // Grok's `--yolo` is a hidden alias of the documented
        // `--always-approve`; either spelling satisfies the request.
        let present: &[&str] = match harness {
            ManagedHarness::Kimi => &["--yolo", "-y", "--yes", "--auto-approve", "--auto"],
            ManagedHarness::CommandCode => &["--yolo", "--dangerously-skip-permissions"],
            // A narrower native trust set is an explicit user choice and must
            // never be widened by the wrapper.
            ManagedHarness::Kiro => &["--trust-all-tools", "-a", "--trust-tools"],
            ManagedHarness::Grok => &["--yolo", "--always-approve"],
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
        && !has_native_session_selector(harness, native_args)
        && !noninteractive_invocation(harness, native_args)
}

fn noninteractive_invocation(harness: ManagedHarness, args: &[OsString]) -> bool {
    match harness {
        ManagedHarness::Claude => has_flag(args, &["--print", "-p"]),
        ManagedHarness::Codex => first_arg_is(args, "exec"),
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => first_arg_is(args, "run"),
        ManagedHarness::Crush => first_arg_is(args, "run"),
        ManagedHarness::Pi | ManagedHarness::Omp => has_flag(args, &["--print", "-p"]),
        ManagedHarness::Kimi => has_flag(args, &["--prompt", "-p"]),
        ManagedHarness::CommandCode => has_flag(args, &["--print", "-p"]),
        ManagedHarness::Kiro | ManagedHarness::KiroV3 => has_flag(args, &["--no-interactive"]),
        ManagedHarness::Grok => {
            has_flag(args, &["--single", "-p", "--prompt-file", "--prompt-json"])
        }
        // `--prompt` is a documented alias of `--print`. `--prompt-interactive`
        // / `-i` is NOT: it seeds a prompt and then keeps the session open, so
        // it stays adoptable.
        ManagedHarness::Antigravity => has_flag(args, &["--print", "-p", "--prompt"]),
    }
}

fn launch_mode(harness: ManagedHarness, args: &[OsString]) -> LaunchMode {
    // Kiro's `-v` is verbose (its version short flag is `-V`), so the
    // generic version-flag check must not send `kiro-cli -v` through
    // unmanaged.
    let version_flags: &[&str] = if matches!(harness, ManagedHarness::Kiro | ManagedHarness::KiroV3)
    {
        &["--help", "-h", "--version", "-V", "--help-all"]
    } else {
        &["--help", "-h", "--version", "-v"]
    };
    if has_flag(args, version_flags)
        || has_flag(args, &["--no-session", "--no-session-persistence"])
    {
        return LaunchMode::Passthrough;
    }
    if harness == ManagedHarness::CommandCode && has_flag(args, &["--list-models", "--ide-setup"]) {
        return LaunchMode::Passthrough;
    }
    if matches!(harness, ManagedHarness::Kiro | ManagedHarness::KiroV3) {
        // An unknown non-v2 engine remains passthrough rather than being
        // assigned to either incompatible adapter. Headless runs and one-shot
        // list/delete flags are not session-bearing.
        if harness == ManagedHarness::Kiro && kiro_selects_non_default_engine(args)
            || has_flag(
                args,
                &[
                    "--no-interactive",
                    "--list-sessions",
                    "-l",
                    "--list-models",
                    "--delete-session",
                    "-d",
                ],
            )
        {
            return LaunchMode::Passthrough;
        }
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
        // Beta subcommands, verified on `opencode2 v0.0.0-beta-18999`
        // (`opencode2 --help`). `run` stays session-bearing (see
        // `noninteractive_invocation`); `mini` is the minimal interactive
        // UI and also stays session-bearing.
        ManagedHarness::OpenCode2 => [
            "upgrade", "acp", "api", "debug", "console", "auth", "mcp", "plugin", "models",
            "stats", "export", "import", "service", "pair", "serve",
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
        ManagedHarness::CommandCode => [
            "info",
            "status",
            "help",
            "whoami",
            "update",
            "feedback",
            "taste",
            "learn-taste",
            "mcp",
            "skills",
            "mods",
            "login",
            "logout",
        ]
        .as_slice(),
        // Every root command except `chat` in kiro-cli 2.16.2. Bare and
        // flags-only invocations open chat and remain session-bearing.
        ManagedHarness::Kiro | ManagedHarness::KiroV3 => [
            "debug",
            "settings",
            "setup",
            "update",
            "diagnostic",
            "init",
            "theme",
            "issue",
            "login",
            "logout",
            "whoami",
            "profile",
            "user",
            "doctor",
            "launch",
            "quit",
            "restart",
            "integrations",
            "translate",
            "dashboard",
            "mcp",
            "inline",
            "agent",
            "acp",
            "help",
        ]
        .as_slice(),
        // `agent` covers the stdio/headless/serve/leader runners, which manage
        // their own session lifecycles and must not receive selectors.
        ManagedHarness::Grok => [
            "agent",
            "completions",
            "dashboard",
            "doctor",
            "export",
            "help",
            "inspect",
            "leader",
            "login",
            "logout",
            "mcp",
            "memory",
            "models",
            "plugin",
            "sessions",
            "setup",
            "trace",
            "update",
            "version",
            "v",
            "worktree",
            "wrap",
        ]
        .as_slice(),
        ManagedHarness::Antigravity => [
            "agent",
            "agents",
            "changelog",
            "help",
            "install",
            "models",
            "plugin",
            "plugins",
            "update",
        ]
        .as_slice(),
    };
    let first = if matches!(harness, ManagedHarness::Kiro | ManagedHarness::KiroV3) {
        kiro_root_subcommand(args)
    } else {
        args.first().and_then(|arg| arg.to_str())
    };
    if first.is_some_and(|value| utility.contains(&value)) {
        LaunchMode::Passthrough
    } else {
        LaunchMode::Session
    }
}

/// Whether the caller supplied a native resume, continue, fork, or session
/// selector. Wrapper recovery must not override an explicit native choice.
#[must_use]
pub fn has_native_session_selector(harness: ManagedHarness, args: &[OsString]) -> bool {
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
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
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
        ManagedHarness::CommandCode => has_flag(
            args,
            &[
                "--session",
                "--resume",
                "--sessions",
                "-r",
                "--continue",
                "-c",
                "--fork-session",
            ],
        ),
        ManagedHarness::Kiro | ManagedHarness::KiroV3 => has_flag(
            args,
            &["--resume", "-r", "--resume-id", "--resume-picker", "--list"],
        ),
        // A bare `--resume` opens Grok's native session picker; that is still
        // an explicit user choice. `--fork-session` modifies how the explicit
        // resume/continue selector behaves and never appears alone.
        ManagedHarness::Grok => has_flag(
            args,
            &[
                "--resume",
                "-r",
                "--continue",
                "-c",
                "--session-id",
                "-s",
                "--fork-session",
            ],
        ),
        // `--continue` / `-c` resumes the most recent conversation without
        // naming one; that is still an explicit user choice, so nothing may be
        // injected over it.
        ManagedHarness::Antigravity => has_flag(args, &["--conversation", "--continue", "-c"]),
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
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
            flag_value(args, &["--session", "-s"])
        }
        ManagedHarness::Pi => flag_value(args, &["--session", "--session-id"]),
        ManagedHarness::Crush => flag_value(args, &["--session", "-s"]),
        ManagedHarness::Omp => flag_value(args, &["--resume", "-r"]),
        // A bare `--session`/`--resume` opens the picker: `flag_value`
        // returns `None` when no value follows, as intended.
        ManagedHarness::Kimi => flag_value(args, &["--session", "-S", "--resume", "-r"]),
        ManagedHarness::CommandCode => flag_value(args, &["--session", "--resume", "-r"])
            .filter(|value| Uuid::parse_str(value).is_ok()),
        ManagedHarness::Kiro | ManagedHarness::KiroV3 => flag_value(args, &["--resume-id"]),
        ManagedHarness::Grok => flag_value(args, &["--resume", "-r", "--session-id", "-s"]),
        // A bare `--continue` names no conversation: the id is only known
        // after the fact, from the conversation store.
        ManagedHarness::Antigravity => flag_value(args, &["--conversation"]),
    }
}

fn first_arg_is(args: &[OsString], expected: &str) -> bool {
    args.first().and_then(|value| value.to_str()) == Some(expected)
}

fn kiro_root_subcommand(args: &[OsString]) -> Option<&str> {
    let mut index = 0;
    while index < args.len() {
        let value = args.get(index)?.to_str()?;
        if matches!(value, "--agent" | "--resume-id") {
            index += 2;
            continue;
        }
        if value.starts_with("--agent=") || value.starts_with("--resume-id=") {
            index += 1;
            continue;
        }
        if value.starts_with('-') {
            index += 1;
            continue;
        }
        return Some(value);
    }
    None
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
        // The beta channel keeps v1's `opencode.db` filename (other channels
        // get `opencode-<channel>.db`), so both harnesses share one store.
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => {
            value("XDG_DATA_HOME").map(|dir| dir.join("opencode"))
        }
        ManagedHarness::Pi => value("PI_CODING_AGENT_SESSION_DIR")
            .or_else(|| value("PI_CODING_AGENT_DIR").map(|dir| dir.join("sessions"))),
        ManagedHarness::Crush => None,
        ManagedHarness::Omp => value("PI_CODING_AGENT_DIR").map(|dir| dir.join("sessions")),
        // Sessions live under `<KIMI_CODE_HOME>/sessions/<bucket>/<id>/`.
        ManagedHarness::Kimi => value("KIMI_CODE_HOME").map(|dir| dir.join("sessions")),
        // Command Code documents no session-root override. Its user store is
        // rooted below HOME and remains isolated when the wrapper runs with a
        // configured host home.
        ManagedHarness::CommandCode => None,
        ManagedHarness::Kiro => value("KIRO_HOME").map(|dir| dir.join("sessions/cli")),
        ManagedHarness::KiroV3 => value("KIRO_HOME").map(|dir| dir.join("sessions")),
        // Sessions live under `<GROK_HOME>/sessions/<encoded-cwd>/<id>/`.
        ManagedHarness::Grok => value("GROK_HOME").map(|dir| dir.join("sessions")),
        // `agy` exposes no environment override for its conversation store.
        ManagedHarness::Antigravity => None,
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
        assert!(!allows_native_session_adoption(
            ManagedHarness::CommandCode,
            &[OsString::from("--print"), OsString::from("continue here")]
        ));
    }

    #[test]
    fn opencode2_shares_v1_session_contract_with_its_own_binary() {
        for name in ["opencode2", "opencode-v2", "open-code2"] {
            assert_eq!(
                ManagedHarness::from_name(name),
                Some(ManagedHarness::OpenCode2)
            );
        }
        let beta = ManagedHarness::OpenCode2;
        // Same store, same kind, same flags — only the executable differs,
        // so `run opencode2` resumes v1 sessions and vice versa.
        assert_eq!(beta.executable(), "opencode2");
        assert_eq!(beta.as_str(), "opencode2");
        assert_eq!(beta.agent_kind(), AgentKind::OpenCode);
        assert_eq!(ManagedHarness::OpenCode.agent_kind(), AgentKind::OpenCode);

        let plan = build_launch_plan(
            beta,
            None,
            vec![OsString::from("run"), OsString::from("continue here")],
            Some("shared-id"),
        )
        .unwrap();
        assert_eq!(
            strings(&plan.args),
            ["run", "--session", "shared-id", "continue here"]
        );
        assert_eq!(plan.expected_session_id.as_deref(), Some("shared-id"));

        let mut yolo = Vec::new();
        apply_yolo(beta, &mut yolo);
        apply_yolo(beta, &mut yolo);
        assert_eq!(strings(&yolo), ["--auto"]);

        assert!(allows_native_session_adoption(
            beta,
            &[OsString::from("--auto")]
        ));
        assert!(!allows_native_session_adoption(
            beta,
            &[OsString::from("run"), OsString::from("continue here")]
        ));

        for utility in ["service", "api", "auth"] {
            let plan =
                build_launch_plan(beta, None, vec![OsString::from(utility)], Some("shared-id"))
                    .unwrap();
            assert_eq!(plan.mode, LaunchMode::Passthrough, "{utility}");
        }
        // `mini` is the interactive UI, not a utility: it stays
        // session-bearing.
        let mini =
            build_launch_plan(beta, None, vec![OsString::from("mini")], Some("shared-id")).unwrap();
        assert_eq!(mini.mode, LaunchMode::Session);
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
            (ManagedHarness::CommandCode, Some("--yolo")),
            (ManagedHarness::Grok, Some("--yolo")),
            (
                ManagedHarness::Antigravity,
                Some("--dangerously-skip-permissions"),
            ),
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
    fn command_code_resumes_exactly_and_preserves_native_arguments() {
        let fresh = build_launch_plan(
            ManagedHarness::CommandCode,
            None,
            vec![OsString::from("--model"), OsString::from("model-id")],
            None,
        )
        .unwrap();
        assert_eq!(strings(&fresh.args), ["--model", "model-id"]);
        assert_eq!(fresh.expected_session_id, None);

        let id = "7c1d5698-204a-4c0f-ae9c-43db7fc4e41d";
        let resumed = build_launch_plan(
            ManagedHarness::CommandCode,
            None,
            vec![OsString::from("--model"), OsString::from("model-id")],
            Some(id),
        )
        .unwrap();
        assert_eq!(
            strings(&resumed.args),
            ["--model", "model-id", "--session", id]
        );
        assert_eq!(resumed.expected_session_id.as_deref(), Some(id));
    }

    #[test]
    fn command_code_explicit_selectors_and_utilities_are_not_overridden() {
        let id = "2cce5126-f57d-4ddd-8f66-e5bb409f60db";
        let exact = build_launch_plan(
            ManagedHarness::CommandCode,
            None,
            vec![OsString::from("--session"), OsString::from(id)],
            Some("7c1d5698-204a-4c0f-ae9c-43db7fc4e41d"),
        )
        .unwrap();
        assert_eq!(strings(&exact.args), ["--session", id]);
        assert_eq!(exact.expected_session_id.as_deref(), Some(id));

        let named = build_launch_plan(
            ManagedHarness::CommandCode,
            None,
            vec![OsString::from("--resume=auth refactor")],
            Some("7c1d5698-204a-4c0f-ae9c-43db7fc4e41d"),
        )
        .unwrap();
        assert_eq!(strings(&named.args), ["--resume=auth refactor"]);
        assert_eq!(named.expected_session_id, None);

        for args in [
            vec![OsString::from("mcp"), OsString::from("list")],
            vec![OsString::from("--no-session")],
            vec![OsString::from("--list-models")],
        ] {
            let plan = build_launch_plan(ManagedHarness::CommandCode, None, args.clone(), Some(id))
                .unwrap();
            assert_eq!(plan.args, args);
            assert_eq!(plan.mode, LaunchMode::Passthrough);
        }
    }

    #[test]
    fn command_code_yolo_recognizes_only_equivalent_dangerous_modes() {
        let mut alias = vec![OsString::from("--dangerously-skip-permissions")];
        apply_yolo(ManagedHarness::CommandCode, &mut alias);
        assert_eq!(strings(&alias), ["--dangerously-skip-permissions"]);

        let mut narrower = vec![OsString::from("--auto-accept")];
        apply_yolo(ManagedHarness::CommandCode, &mut narrower);
        assert_eq!(strings(&narrower), ["--auto-accept", "--yolo"]);
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
    fn grok_generates_then_resumes_native_session() {
        let fresh = build_launch_plan(ManagedHarness::Grok, None, vec![], None).unwrap();
        let id = fresh.expected_session_id.clone().unwrap();
        assert_eq!(strings(&fresh.args), ["--session-id", id.as_str()]);

        let resumed = build_launch_plan(
            ManagedHarness::Grok,
            None,
            vec![OsString::from("--model"), OsString::from("grok-4.5")],
            Some(&id),
        )
        .unwrap();
        assert_eq!(
            strings(&resumed.args),
            ["--model", "grok-4.5", "--resume", id.as_str()]
        );
    }

    #[test]
    fn grok_explicit_selector_always_wins_including_bare_picker_and_fork() {
        for native in [
            vec![OsString::from("--resume"), OsString::from("chosen")],
            vec![OsString::from("--session-id=chosen")],
            vec![OsString::from("-c")],
            vec![OsString::from("--fork-session")],
            // Bare `--resume` opens the native picker; still an explicit
            // choice, so nothing may be injected.
            vec![OsString::from("--resume")],
        ] {
            let plan =
                build_launch_plan(ManagedHarness::Grok, None, native.clone(), Some("linked"))
                    .unwrap();
            assert_eq!(plan.args, native, "{native:?} must stay byte-identical");
            assert_ne!(plan.expected_session_id.as_deref(), Some("linked"));
        }
    }

    #[test]
    fn grok_utility_subcommands_are_passed_through() {
        for utility in ["agent", "sessions", "login", "export", "doctor", "wrap"] {
            let plan = build_launch_plan(
                ManagedHarness::Grok,
                None,
                vec![OsString::from(utility)],
                Some("linked"),
            )
            .unwrap();
            assert_eq!(plan.mode, LaunchMode::Passthrough, "{utility}");
            assert_eq!(strings(&plan.args), [utility]);
        }
    }

    #[test]
    fn grok_noninteractive_prompt_blocks_adoption() {
        for args in [
            vec![OsString::from("-p"), OsString::from("summarize")],
            vec![OsString::from("--single"), OsString::from("summarize")],
            vec![OsString::from("--prompt-file"), OsString::from("p.md")],
        ] {
            assert!(!allows_native_session_adoption(ManagedHarness::Grok, &args));
        }
        assert!(allows_native_session_adoption(
            ManagedHarness::Grok,
            &[OsString::from("--model"), OsString::from("grok-4.5")]
        ));
    }

    #[test]
    fn grok_yolo_respects_the_always_approve_alias() {
        for already in ["--yolo", "--always-approve"] {
            let mut args = vec![OsString::from(already)];
            apply_yolo(ManagedHarness::Grok, &mut args);
            assert_eq!(strings(&args), [already], "{already} must not duplicate");
        }
    }

    #[test]
    fn grok_build_name_is_an_alias_for_grok() {
        for name in ["grok", "grok-build"] {
            assert_eq!(ManagedHarness::from_name(name), Some(ManagedHarness::Grok));
        }
    }

    #[test]
    fn grok_home_environment_override_points_at_sessions_root() {
        let get = |name: &str| (name == "GROK_HOME").then(|| OsString::from("/stores/grok"));
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Grok, get).as_deref(),
            Some(std::path::Path::new("/stores/grok/sessions"))
        );
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

    #[test]
    fn kiro_names_parse_to_the_v2_adapter() {
        for name in ["kiro", "kiro-cli"] {
            assert_eq!(ManagedHarness::from_name(name), Some(ManagedHarness::Kiro));
        }
        assert_eq!(ManagedHarness::Kiro.executable(), "kiro-cli");
    }

    #[test]
    fn kiro_fresh_and_linked_launches_preserve_native_arguments() {
        let fresh = build_launch_plan(
            ManagedHarness::Kiro,
            None,
            vec![OsString::from("--model"), OsString::from("sonnet")],
            None,
        )
        .unwrap();
        assert_eq!(strings(&fresh.args), ["--model", "sonnet"]);
        assert_eq!(fresh.expected_session_id, None);

        let linked = build_launch_plan(
            ManagedHarness::Kiro,
            None,
            vec![OsString::from("--model"), OsString::from("sonnet")],
            Some("3f6d1c2a-0000-4000-8000-000000000aaa"),
        )
        .unwrap();
        assert_eq!(
            strings(&linked.args),
            [
                "--model",
                "sonnet",
                "--resume-id",
                "3f6d1c2a-0000-4000-8000-000000000aaa"
            ]
        );

        let explicit = build_launch_plan(
            ManagedHarness::KiroV3,
            None,
            vec![
                OsString::from("--resume-id"),
                OsString::from("sess_5f8f43ff-d4b0-4b46-9320-f2f756ced54b"),
            ],
            Some("sess_c3774f9d-269e-40d1-aa02-2bb0c0817b4e"),
        )
        .unwrap();
        assert_eq!(
            strings(&explicit.args),
            [
                "--v3",
                "--resume-id",
                "sess_5f8f43ff-d4b0-4b46-9320-f2f756ced54b"
            ]
        );
    }

    #[test]
    fn kiro_v3_fresh_and_linked_launches_select_only_the_v3_store() {
        let fresh = build_launch_plan(
            ManagedHarness::KiroV3,
            None,
            vec![OsString::from("--model"), OsString::from("sonnet")],
            None,
        )
        .unwrap();
        assert_eq!(strings(&fresh.args), ["--v3", "--model", "sonnet"]);
        assert_eq!(fresh.expected_session_id, None);

        let linked = build_launch_plan(
            ManagedHarness::KiroV3,
            None,
            vec![OsString::from("--v3"), OsString::from("--mode=vibe")],
            Some("sess_c3774f9d-269e-40d1-aa02-2bb0c0817b4e"),
        )
        .unwrap();
        assert_eq!(
            strings(&linked.args),
            [
                "--v3",
                "--mode=vibe",
                "--resume-id",
                "sess_c3774f9d-269e-40d1-aa02-2bb0c0817b4e"
            ]
        );
    }

    #[test]
    fn kiro_engine_selection_distinguishes_v2_v3_and_unknown_values() {
        assert!(kiro_selects_v3_engine(&[OsString::from("--v3")]));
        assert!(kiro_selects_v3_engine(&[OsString::from("--mode=vibe")]));
        assert!(kiro_selects_v3_engine(&[
            OsString::from("--agent-engine"),
            OsString::from("v3")
        ]));
        assert!(kiro_selects_v2_engine(&[OsString::from(
            "--agent-engine=v2"
        )]));
        assert!(!kiro_selects_v3_engine(&[OsString::from(
            "--agent-engine=future"
        )]));
        assert!(kiro_selects_non_default_engine(&[OsString::from(
            "--agent-engine=future"
        )]));
    }

    #[test]
    fn kiro_explicit_selectors_and_non_v2_engines_are_never_overridden() {
        for native in [
            vec![OsString::from("--resume")],
            vec![
                OsString::from("--resume-id"),
                OsString::from("3f6d1c2a-0000-4000-8000-000000000aaa"),
            ],
            vec![OsString::from("--resume-picker")],
            vec![OsString::from("--v3")],
            vec![OsString::from("--agent-engine"), OsString::from("v1")],
        ] {
            let plan = build_launch_plan(
                ManagedHarness::Kiro,
                None,
                native.clone(),
                Some("3f6d1c2a-0000-4000-8000-000000000bbb"),
            )
            .unwrap();
            assert_eq!(plan.args, native, "{native:?} must stay byte-identical");
        }
    }

    #[test]
    fn kiro_utilities_are_passthrough_even_after_global_flags() {
        for native in [
            vec![OsString::from("login")],
            vec![OsString::from("-vv"), OsString::from("doctor")],
            vec![
                OsString::from("--agent"),
                OsString::from("reviewer"),
                OsString::from("mcp"),
            ],
        ] {
            let plan = build_launch_plan(
                ManagedHarness::Kiro,
                None,
                native.clone(),
                Some("3f6d1c2a-0000-4000-8000-000000000aaa"),
            )
            .unwrap();
            assert_eq!(plan.mode, LaunchMode::Passthrough, "{native:?}");
            assert_eq!(plan.args, native);
        }
        assert_eq!(
            build_launch_plan(
                ManagedHarness::Kiro,
                None,
                vec![OsString::from("chat")],
                None,
            )
            .unwrap()
            .mode,
            LaunchMode::Session
        );
    }

    #[test]
    fn kiro_yolo_maps_only_to_the_v2_permission_flag() {
        let mut args = Vec::new();
        apply_yolo(ManagedHarness::Kiro, &mut args);
        assert_eq!(strings(&args), ["--trust-all-tools"]);

        for already in ["--trust-all-tools", "-a", "--trust-tools=fs_read"] {
            let mut args = vec![OsString::from(already)];
            apply_yolo(ManagedHarness::Kiro, &mut args);
            assert_eq!(strings(&args), [already]);
        }

        let mut v3 = vec![OsString::from("--v3")];
        apply_yolo(ManagedHarness::Kiro, &mut v3);
        assert_eq!(strings(&v3), ["--v3"]);

        let mut managed_v3 = vec![OsString::from("--v3")];
        apply_yolo(ManagedHarness::KiroV3, &mut managed_v3);
        assert_eq!(strings(&managed_v3), ["--v3"]);
    }

    #[test]
    fn kiro_noninteractive_and_one_shot_modes_are_passthrough() {
        for native in [
            vec![OsString::from("--no-interactive"), OsString::from("hi")],
            vec![OsString::from("--list-sessions")],
            vec![OsString::from("--list-models")],
            vec![
                OsString::from("--delete-session"),
                OsString::from("3f6d1c2a-0000-4000-8000-000000000aaa"),
            ],
        ] {
            assert_eq!(
                build_launch_plan(ManagedHarness::Kiro, None, native, None)
                    .unwrap()
                    .mode,
                LaunchMode::Passthrough
            );
        }
    }

    #[test]
    fn kiro_home_override_points_at_the_cli_session_store() {
        let get = |name: &str| (name == "KIRO_HOME").then(|| OsString::from("/stores/kiro"));
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Kiro, get).as_deref(),
            Some(std::path::Path::new("/stores/kiro/sessions/cli"))
        );
        let get = |name: &str| (name == "KIRO_HOME").then(|| OsString::from("/stores/kiro"));
        assert_eq!(
            environment_session_dir_with(ManagedHarness::KiroV3, get).as_deref(),
            Some(std::path::Path::new("/stores/kiro/sessions"))
        );
    }

    #[test]
    fn antigravity_names_parse_to_one_variant() {
        for name in ["antigravity", "antigravity-cli", "agy"] {
            assert_eq!(
                ManagedHarness::from_name(name),
                Some(ManagedHarness::Antigravity)
            );
        }
        assert_eq!(ManagedHarness::Antigravity.executable(), "agy");
    }

    /// `agy` rejects a caller-chosen id for a new conversation, so a fresh
    /// launch must leave argv untouched even though the harness is managed.
    #[test]
    fn antigravity_fresh_launch_injects_no_selector() {
        let plan = build_launch_plan(
            ManagedHarness::Antigravity,
            None,
            vec![OsString::from("--model"), OsString::from("gemini-3-pro")],
            None,
        )
        .unwrap();
        assert_eq!(strings(&plan.args), ["--model", "gemini-3-pro"]);
        assert_eq!(plan.expected_session_id, None);
        assert_eq!(plan.mode, LaunchMode::Session);
    }

    #[test]
    fn antigravity_resume_appends_conversation_selector_after_user_arguments() {
        let plan = build_launch_plan(
            ManagedHarness::Antigravity,
            None,
            vec![OsString::from("--effort"), OsString::from("high")],
            Some("a0d5ac62-2501-4780-b783-76d159c56cb3"),
        )
        .unwrap();
        assert_eq!(
            strings(&plan.args),
            [
                "--effort",
                "high",
                "--conversation",
                "a0d5ac62-2501-4780-b783-76d159c56cb3"
            ]
        );
        assert_eq!(
            plan.expected_session_id.as_deref(),
            Some("a0d5ac62-2501-4780-b783-76d159c56cb3")
        );
    }

    #[test]
    fn antigravity_explicit_selector_always_wins() {
        for native in [
            vec![OsString::from("--conversation"), OsString::from("chosen")],
            vec![OsString::from("--conversation=chosen")],
            // `--continue` names no conversation but is still the user's
            // explicit choice, so nothing may be injected over it.
            vec![OsString::from("--continue")],
            vec![OsString::from("-c")],
        ] {
            let plan = build_launch_plan(
                ManagedHarness::Antigravity,
                None,
                native.clone(),
                Some("linked"),
            )
            .unwrap();
            assert_eq!(plan.args, native, "{native:?} must stay byte-identical");
            assert_ne!(plan.expected_session_id.as_deref(), Some("linked"));
        }
    }

    #[test]
    fn antigravity_utility_subcommands_are_passed_through() {
        for utility in [
            "models",
            "plugin",
            "update",
            "agents",
            "install",
            "changelog",
        ] {
            let plan = build_launch_plan(
                ManagedHarness::Antigravity,
                None,
                vec![OsString::from(utility)],
                Some("linked"),
            )
            .unwrap();
            assert_eq!(plan.mode, LaunchMode::Passthrough, "{utility}");
            assert_eq!(strings(&plan.args), [utility]);
        }
    }

    /// `--print` and its `--prompt` alias answer and exit; `-i`
    /// (`--prompt-interactive`) seeds a prompt and keeps the session open, so
    /// it must stay adoptable.
    #[test]
    fn antigravity_print_blocks_adoption_but_interactive_prompt_does_not() {
        for blocked in [
            vec![OsString::from("-p"), OsString::from("summarize")],
            vec![OsString::from("--print"), OsString::from("summarize")],
            vec![OsString::from("--prompt"), OsString::from("summarize")],
        ] {
            assert!(!allows_native_session_adoption(
                ManagedHarness::Antigravity,
                &blocked
            ));
        }
        assert!(allows_native_session_adoption(
            ManagedHarness::Antigravity,
            &[OsString::from("-i"), OsString::from("start here")]
        ));
    }
}
