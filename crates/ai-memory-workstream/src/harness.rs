//! Native command planning without filtering harness arguments.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use ai_memory_core::AgentKind;
use anyhow::{Result, anyhow, bail};
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
    build_launch_plan_with_env(
        harness,
        executable,
        native_args,
        linked_session_id,
        &[],
        None,
    )
}

/// Where a launch runs: the home the harness resolves `~` against and the
/// directory it starts in.
#[derive(Debug, Clone, Copy)]
pub struct LaunchRoots<'a> {
    /// The native home.
    pub home: &'a Path,
    /// The harness's working directory.
    pub cwd: &'a Path,
}

/// [`build_launch_plan`] with `--env`/`--env-file` overrides layered in front
/// of the real process environment for native session-store resolution
/// (e.g. `CLAUDE_CONFIG_DIR`).
///
/// A launch's own environment overrides must be visible here, not only to the
/// spawned child: `ai-memory run` resolves the native transcript root from
/// this same variable, so a caller-scoped override that only reached the
/// child process would make the two disagree about where the session lives
/// (see the `CLAUDE_CONFIG_DIR` note in `docs/managed-workstreams.md`).
///
/// `roots` name the native home and the launch directory. OMP needs the home
/// (a named profile, `PI_CONFIG_DIR`, or an XDG data dir) and Crush both (its
/// data directory is found from the launch directory, see
/// [`crush_data_dir`]); without them those stores fall back to the adapter's
/// default root.
pub fn build_launch_plan_with_env(
    harness: ManagedHarness,
    executable: Option<OsString>,
    native_args: Vec<OsString>,
    linked_session_id: Option<&str>,
    env_overrides: &[(String, String)],
    roots: Option<LaunchRoots<'_>>,
) -> Result<LaunchPlan> {
    let program = executable.unwrap_or_else(|| OsString::from(harness.executable()));
    let mut args = native_args;
    let get = |name: &str| {
        env_overrides
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| OsString::from(value))
            .or_else(|| std::env::var_os(name))
    };
    let session_dir = match harness {
        ManagedHarness::Pi | ManagedHarness::Omp => flag_path(&args, &["--session-dir"]),
        ManagedHarness::Crush => flag_path(&args, &["--data-dir", "-D"]),
        _ => None,
    }
    .or_else(|| {
        let omp_profile = match harness {
            ManagedHarness::Omp => omp_profile_flag(&args),
            _ => None,
        };
        environment_session_dir_with(
            harness,
            roots.map(|roots| roots.home),
            omp_profile.as_deref(),
            get,
        )
    })
    .or_else(|| match (harness, roots) {
        (ManagedHarness::Crush, Some(roots)) => Some(crush_data_dir(roots.cwd, roots.home, get)),
        _ => None,
    });
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

/// A harness home variable's directory, or `None` when it is unset or blank.
///
/// Blank (empty or whitespace-only) counts as unset on purpose: an
/// exported-but-empty variable is far more often an unset shell expansion than
/// a request to use the filesystem root or a directory named by whitespace.
/// Session import and the hook/MCP installers share this one rule, so a blank
/// override never sends one to the default home and the other to `<cwd>/ /`.
pub fn env_dir_override(value: Option<OsString>) -> Option<PathBuf> {
    let value = value?;
    if value.to_str().is_some_and(|text| text.trim().is_empty()) {
        return None;
    }
    Some(PathBuf::from(value))
}

/// The profile an OMP command line selects with a leading `--profile` (after
/// an optional `launch` or `acp`), which OMP ranks above `OMP_PROFILE`.
///
/// Only the leading position is read. OMP stops extracting global flags at a
/// subcommand (`omp grep --profile x` greps for `--profile`) and at `--`, and a
/// string flag such as `--system-prompt` takes a following `--profile` as its
/// value; telling those apart later in the line needs OMP's own flag tables.
/// A `--profile` anywhere else is left to the environment instead of guessed,
/// and so is a leading one that a later `--profile` might override.
pub fn omp_profile_flag(args: &[OsString]) -> Option<String> {
    let args: Vec<&str> = args.iter().map(|arg| arg.to_str()).collect::<Option<_>>()?;
    let mut rest = match args.first() {
        Some(&"launch" | &"acp") => &args[1..],
        _ => &args[..],
    };
    let mut profile = None;
    loop {
        match rest {
            [flag, value, tail @ ..] if *flag == "--profile" => {
                profile = Some(*value);
                rest = tail;
            }
            [flag, tail @ ..] if flag.starts_with("--profile=") => {
                profile = flag.strip_prefix("--profile=");
                rest = tail;
            }
            _ => break,
        }
    }
    let is_profile_flag = |arg: &&str| *arg == "--profile" || arg.starts_with("--profile=");
    if rest.iter().any(is_profile_flag) {
        return None;
    }
    profile
        .filter(|value| !value.is_empty() && !value.starts_with('-'))
        .map(str::to_owned)
}

/// The variables that relocate a harness's native store, the ones
/// [`build_launch_plan_with_env`] resolves its session directory from.
pub fn store_override_vars(harness: ManagedHarness) -> &'static [&'static str] {
    match harness {
        ManagedHarness::Claude => &["CLAUDE_CONFIG_DIR"],
        ManagedHarness::Codex => &["CODEX_HOME"],
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => &["XDG_DATA_HOME"],
        ManagedHarness::Pi => &["PI_CODING_AGENT_SESSION_DIR", "PI_CODING_AGENT_DIR"],
        ManagedHarness::Omp => &[
            "PI_CODING_AGENT_SESSION_DIR",
            "PI_CODING_AGENT_DIR",
            "PI_CONFIG_DIR",
            "XDG_DATA_HOME",
        ],
        ManagedHarness::Kimi => &["KIMI_CODE_HOME"],
        ManagedHarness::Kiro | ManagedHarness::KiroV3 => &["KIRO_HOME"],
        ManagedHarness::Grok => &["GROK_HOME"],
        ManagedHarness::Crush | ManagedHarness::CommandCode | ManagedHarness::Antigravity => &[],
    }
}

/// OMP's active profile, resolved the way OMP resolves it
/// (`normalizeProfileName` and `resolveProfileEnv` in its
/// `pi-utils/src/dirs.ts`, checked against 18.2.5): an explicit `--profile`
/// wins, then `OMP_PROFILE` whenever it is set, even to an empty value, and
/// only then the legacy `PI_PROFILE`. The name is trimmed, and an empty,
/// whitespace-only or `default` name selects the default profile (`None`).
///
/// # Errors
/// Returns an error for a name OMP itself refuses, so ai-memory never wires a
/// profile directory OMP will not load.
fn omp_profile(
    explicit: Option<&str>,
    get: impl Fn(&str) -> Option<OsString>,
) -> Result<Option<String>> {
    let raw = match explicit {
        Some("") => bail!("--profile requires a profile name"),
        Some(name) => name.to_owned(),
        None => match get("OMP_PROFILE").or_else(|| get("PI_PROFILE")) {
            Some(value) => value
                .into_string()
                .map_err(|value| anyhow!("Invalid OMP profile {value:?}: not valid UTF-8"))?,
            None => return Ok(None),
        },
    };
    normalize_omp_profile(&raw)
}

/// OMP's `normalizeProfileName`: trimmed, with empty and `default` meaning the
/// default profile.
fn normalize_omp_profile(raw: &str) -> Result<Option<String>> {
    let name = raw.trim();
    if name.is_empty() || name == "default" {
        return Ok(None);
    }
    if !is_valid_omp_profile_name(name) {
        bail!(
            "Invalid OMP profile \"{raw}\". Profile names must match ^[a-z0-9][a-z0-9._-]{{0,63}}$, \
             cannot be \".\" or \"..\", cannot end with \".\", and cannot be a Windows reserved \
             device name (CON, PRN, AUX, NUL, COM0-9, LPT0-9, or any of those with an extension)."
        );
    }
    Ok(Some(name.to_owned()))
}

fn is_valid_omp_profile_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let lower_alnum = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let base = name.split('.').next().unwrap_or(name);
    let reserved = matches!(base, "con" | "prn" | "aux" | "nul")
        || (base.len() == 4
            && (base.starts_with("com") || base.starts_with("lpt"))
            && base.as_bytes()[3].is_ascii_digit());
    (1..=64).contains(&bytes.len())
        && lower_alnum(&bytes[0])
        && bytes[1..]
            .iter()
            .all(|byte| lower_alnum(byte) || matches!(byte, b'.' | b'_' | b'-'))
        && !name.ends_with('.')
        && !reserved
}

/// OMP's agent directory, where it loads extensions and `mcp.json` (and keeps
/// sessions unless XDG moves them, see [`omp_sessions_dir`]). A named profile
/// owns `<root>/profiles/<name>/agent` and ignores `PI_CODING_AGENT_DIR`; the
/// default profile honors a non-blank `PI_CODING_AGENT_DIR`, else
/// `<root>/agent`. `<root>` is `<home>/.omp`, renamed by `PI_CONFIG_DIR`.
///
/// # Errors
/// Returns the [`omp_profile`] error for a profile name OMP refuses.
pub fn omp_agent_dir(
    home: &Path,
    explicit_profile: Option<&str>,
    get: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf> {
    Ok(match omp_profile(explicit_profile, &get)? {
        Some(profile) => omp_profile_agent_dir(home, &profile, &get),
        None => omp_default_agent_dir_override(home, &get)
            .unwrap_or_else(|| omp_config_root(home, &get).join("agent")),
    })
}

/// OMP's config root: `<home>/.omp`, or `<home>/<PI_CONFIG_DIR>` when that
/// variable renames it. OMP joins the value under the home with Node's
/// `path.join`, so an absolute value stays under the home and `..` walks up
/// from it, where `PathBuf::join` would replace the home instead.
fn omp_config_root(home: &Path, get: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let name = env_dir_override(get("PI_CONFIG_DIR")).unwrap_or_else(|| PathBuf::from(".omp"));
    let mut root = home.to_path_buf();
    for component in name.components() {
        match component {
            Component::Normal(part) => root.push(part),
            Component::ParentDir => {
                root.pop();
            }
            // Node's `path.win32.join` keeps a drive or UNC prefix as plain
            // segments under the home (`C:\Users\me\server\share\omp`).
            // Appended as text: pushing `D:` would make it a new prefix.
            Component::Prefix(prefix) => {
                let text = prefix.as_os_str().to_string_lossy().into_owned();
                for part in text.split(['\\', '/']).filter(|part| !part.is_empty()) {
                    let path = root.as_mut_os_string();
                    if !path.to_string_lossy().ends_with(['\\', '/']) {
                        path.push(std::path::MAIN_SEPARATOR_STR);
                    }
                    path.push(part);
                }
            }
            Component::RootDir | Component::CurDir => {}
        }
    }
    root
}

/// `path` with `.` dropped and `..` folded lexically, without touching the
/// filesystem: Go's `filepath.Clean`, and the normalizing half of Node's
/// `path.resolve`. A `..` that would climb above the root is dropped.
pub fn clean_path(path: &Path) -> PathBuf {
    let mut clean = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match clean.components().next_back() {
                Some(Component::Normal(_)) => {
                    clean.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => clean.push(".."),
            },
            other => clean.push(other.as_os_str()),
        }
    }
    if clean.as_os_str().is_empty() {
        clean.push(".");
    }
    clean
}

/// Crush's global `crush.json`, resolved as Crush's `GlobalConfig()` does:
/// `$CRUSH_GLOBAL_CONFIG/crush.json`, else `$XDG_CONFIG_HOME/crush/crush.json`,
/// else `~/.config/crush/crush.json`. `get` is the launch environment, and a
/// blank value counts as unset, as for every other harness home; a managed
/// launch drops such a value from Crush too.
pub fn crush_global_config_path(home: &Path, get: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let dir = |name| env_dir_override(get(name));
    if let Some(dir) = dir("CRUSH_GLOBAL_CONFIG") {
        return dir.join("crush.json");
    }
    dir("XDG_CONFIG_HOME")
        .unwrap_or_else(|| home.join(".config"))
        .join("crush")
        .join("crush.json")
}

/// The directory Crush keeps `crush.db` in when no `--data-dir` is given,
/// resolved as Crush's `setDefaults` does: the last `options.data_directory`
/// among its JSON configs, else the closest `.crush` from `cwd` up to the git
/// worktree root (not one directly in the home, and the walk stops at an
/// entry another user owns), else `<cwd>/.crush`. A relative value is taken
/// against `cwd`. `get` is the launch environment.
///
/// A `crushrc` can set the option as well, but reading it means running the
/// user's shell script, so a data directory set only there is not seen; pass
/// `--data-dir` to name it.
pub fn crush_data_dir(cwd: &Path, home: &Path, get: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    let cwd = clean_path(&std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf()));
    let boundary = crate::repository::worktree_root(&cwd).unwrap_or_else(|| cwd.clone());
    // Crush gives up on both upward searches when it cannot stat the start.
    let walk = path_owner(&cwd)
        .ok()
        .map(|owner| (owner, crush_walk_up(&cwd, &boundary)));
    let configured = crush_config_files(&cwd, home, walk.as_ref(), &get)
        .iter()
        .fold(None, |value, file| {
            crush_config_data_directory(file).or(value)
        })
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from);
    let dir = configured
        .or_else(|| {
            let (owner, dirs) = walk.as_ref()?;
            crush_closest_data_dir(dirs, *owner, home)
        })
        .unwrap_or_else(|| cwd.join(".crush"));
    // Crush's `SmartJoin`: a path that starts with a slash is absolute on
    // Windows too.
    let rooted = cfg!(windows) && dir.to_string_lossy().starts_with(['/', '\\']);
    if dir.is_absolute() || rooted {
        clean_path(&dir)
    } else {
        clean_path(&cwd.join(dir))
    }
}

/// [`crush_data_dir`] against ai-memory's own environment, for a store no
/// launch plan named (automatic session discovery).
pub(crate) fn crush_process_data_dir(cwd: &Path, home: &Path) -> PathBuf {
    crush_data_dir(cwd, home, |name| std::env::var_os(name))
}

/// Crush's JSON configs in merge order, later ones winning: the system file,
/// the global config, the global data config, then the project's
/// `crush.json` and `.crush.json` from the walk's top down to `cwd`.
fn crush_config_files(
    cwd: &Path,
    home: &Path,
    walk: Option<&(Option<u32>, Vec<PathBuf>)>,
    get: &impl Fn(&str) -> Option<OsString>,
) -> Vec<PathBuf> {
    // Crush tests these with `!= ""` and reads a relative path from its
    // working directory.
    let set = |name: &str| {
        get(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    let global_data = if let Some(dir) = set("CRUSH_GLOBAL_DATA") {
        dir.join("crush.json")
    } else if let Some(dir) = set("XDG_DATA_HOME") {
        dir.join("crush").join("crush.json")
    } else if cfg!(windows) {
        set("LOCALAPPDATA")
            .unwrap_or_else(|| {
                PathBuf::from(get("USERPROFILE").unwrap_or_default())
                    .join("AppData")
                    .join("Local")
            })
            .join("crush")
            .join("crush.json")
    } else {
        home.join(".local")
            .join("share")
            .join("crush")
            .join("crush.json")
    };
    let mut files = Vec::new();
    if cfg!(not(windows)) {
        files.push(PathBuf::from("/etc/crush/crush.json"));
    }
    files.push(cwd.join(crush_global_config_path(home, get)));
    files.push(cwd.join(global_data));
    if let Some((owner, dirs)) = walk {
        for dir in dirs.iter().rev() {
            for name in ["crush.json", ".crush.json"] {
                let file = dir.join(name);
                if crush_probe(&file, *owner) == CrushProbe::Found {
                    files.push(file);
                }
            }
        }
    }
    files
}

/// `options.data_directory` from one JSON config, when it sets one.
fn crush_config_data_directory(file: &Path) -> Option<String> {
    let raw = std::fs::read(file).ok()?;
    let config = serde_json::from_slice::<serde_json::Value>(&raw).ok()?;
    config
        .get("options")?
        .get("data_directory")?
        .as_str()
        .map(str::to_owned)
}

/// Crush's `LookupClosestBounded(cwd, boundary, ".crush")` over `dirs`.
fn crush_closest_data_dir(dirs: &[PathBuf], owner: Option<u32>, home: &Path) -> Option<PathBuf> {
    for dir in dirs {
        let candidate = dir.join(".crush");
        match crush_probe(&candidate, owner) {
            CrushProbe::Missing => continue,
            CrushProbe::Refused => return None,
            CrushProbe::Found => return (dir != home).then_some(candidate),
        }
    }
    None
}

/// `cwd` and each parent up to `boundary`, compared with symlinks resolved,
/// or up to the filesystem root when `boundary` is not above `cwd` (Crush's
/// `traverseUpBounded`).
fn crush_walk_up(cwd: &Path, boundary: &Path) -> Vec<PathBuf> {
    let resolved = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| clean_path(path));
    let stop = resolved(boundary);
    let mut dirs = Vec::new();
    let mut dir = Some(cwd);
    while let Some(current) = dir {
        dirs.push(current.to_path_buf());
        if resolved(current) == stop {
            break;
        }
        dir = current.parent();
    }
    dirs
}

#[derive(Debug, PartialEq, Eq)]
enum CrushProbe {
    Found,
    Missing,
    Refused,
}

/// Crush's `probeEnt`: an entry owned by someone other than the walk's
/// owner, or one it cannot stat, is refused.
fn crush_probe(path: &Path, owner: Option<u32>) -> CrushProbe {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CrushProbe::Missing,
        Err(_) => CrushProbe::Refused,
        Ok(metadata) if owner.is_none_or(|owner| metadata_owner(&metadata) == Some(owner)) => {
            CrushProbe::Found
        }
        Ok(_) => CrushProbe::Refused,
    }
}

/// The uid Crush compares while walking up (`fsext.Owner`); `None` on
/// Windows, where Crush skips the check.
fn path_owner(path: &Path) -> std::io::Result<Option<u32>> {
    std::fs::metadata(path).map(|metadata| metadata_owner(&metadata))
}

#[cfg(unix)]
fn metadata_owner(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::MetadataExt as _;
    Some(metadata.uid())
}

#[cfg(not(unix))]
fn metadata_owner(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Where OMP keeps sessions when nothing names the session dir: its agent
/// dir's `sessions`, except that on Linux and macOS an agent dir at its
/// default location moves them under `$XDG_DATA_HOME/omp` (a named profile
/// under `$XDG_DATA_HOME/omp/profiles/<name>`) once that directory exists
/// (`DirResolver` in OMP's `dirs.ts`). Extensions and `mcp.json` never move.
fn omp_sessions_dir(
    home: &Path,
    explicit_profile: Option<&str>,
    get: impl Fn(&str) -> Option<OsString>,
    xdg_platform: bool,
    exists: impl Fn(&Path) -> bool,
) -> Result<PathBuf> {
    let profile = omp_profile(explicit_profile, &get)?;
    let agent_dir = omp_agent_dir(home, explicit_profile, &get)?;
    let default_location = match &profile {
        Some(name) => omp_profile_agent_dir(home, name, &get),
        None => omp_config_root(home, &get).join("agent"),
    };
    // OMP compares the override after `path.resolve`, so a relative or
    // `..`-laden spelling of the default location still counts as default.
    let resolved = std::path::absolute(&agent_dir)
        .map(|dir| clean_path(&dir))
        .unwrap_or_else(|_| agent_dir.clone());
    if xdg_platform
        && resolved == clean_path(&default_location)
        && let Some(data) = env_dir_override(get("XDG_DATA_HOME"))
    {
        let base = match &profile {
            Some(name) => data.join("omp").join("profiles").join(name),
            None => data.join("omp"),
        };
        if exists(&base) {
            return Ok(base.join("sessions"));
        }
    }
    Ok(agent_dir.join("sessions"))
}

/// The default profile's `PI_CODING_AGENT_DIR`, unless it is the agent dir a
/// parent OMP derived for its profile and exported to its children. OMP drops
/// such a value (`resolvePreProfileAgentDir` in `dirs.ts`), so a nested launch
/// back on the default profile uses `~/.omp/agent`, not the parent's profile.
fn omp_default_agent_dir_override(
    home: &Path,
    get: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let dir = env_dir_override(get("PI_CODING_AGENT_DIR"))?;
    match inherited_omp_profile(&get) {
        Some(profile) if dir == omp_profile_agent_dir(home, &profile, &get) => None,
        _ => Some(dir),
    }
}

/// The profile the environment names for OMP (`OMP_PROFILE`, else
/// `PI_PROFILE`), or failing that `PI_PROFILE` on its own: the one OMP checks
/// an inherited `PI_CODING_AGENT_DIR` against. Invalid names count as none,
/// as in OMP's `readProfileFromEnvSafe`.
fn inherited_omp_profile(get: impl Fn(&str) -> Option<OsString>) -> Option<String> {
    let normalized =
        |value: Option<OsString>| normalize_omp_profile(value?.to_str()?).ok().flatten();
    normalized(get("OMP_PROFILE").or_else(|| get("PI_PROFILE")))
        .or_else(|| normalized(get("PI_PROFILE")))
}

/// The `OMP_PROFILE` / `PI_PROFILE` values under which ai-memory's resolvers
/// see what `omp --profile <profile>` sees in the environment `get` reads. A
/// named profile needs only `OMP_PROFILE`. `default` selects the default
/// profile but must keep the profile the environment named, which OMP still
/// uses to drop a `PI_CODING_AGENT_DIR` inherited from that profile.
pub fn omp_profile_flag_env(
    profile: &str,
    get: impl Fn(&str) -> Option<OsString>,
) -> Vec<(String, String)> {
    match normalize_omp_profile(profile) {
        Ok(Some(name)) => vec![("OMP_PROFILE".to_string(), name)],
        Ok(None) => vec![
            ("OMP_PROFILE".to_string(), String::new()),
            (
                "PI_PROFILE".to_string(),
                inherited_omp_profile(&get).unwrap_or_default(),
            ),
        ],
        // OMP refuses to start with it; pass it on so auto-wire reports it.
        Err(_) => vec![("OMP_PROFILE".to_string(), profile.to_string())],
    }
}

fn omp_profile_agent_dir(
    home: &Path,
    profile: &str,
    get: impl Fn(&str) -> Option<OsString>,
) -> PathBuf {
    omp_config_root(home, get)
        .join("profiles")
        .join(profile)
        .join("agent")
}

fn environment_session_dir_with(
    harness: ManagedHarness,
    home: Option<&Path>,
    omp_profile_flag: Option<&str>,
    get: impl Fn(&str) -> Option<OsString>,
) -> Option<PathBuf> {
    let value = |name| env_dir_override(get(name));
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
        // OMP reads `PI_CODING_AGENT_SESSION_DIR` as its `--session-dir`
        // default. A named profile needs the home to resolve, and an invalid
        // one makes OMP refuse to start, so neither has a store to point at.
        ManagedHarness::Omp => value("PI_CODING_AGENT_SESSION_DIR").or_else(|| match home {
            Some(home) => omp_sessions_dir(
                home,
                omp_profile_flag,
                &get,
                cfg!(any(target_os = "linux", target_os = "macos")),
                Path::exists,
            )
            .ok()
            // The adapter's default root already covers an unmoved store.
            .filter(|dir| *dir != home.join(".omp").join("agent").join("sessions")),
            None => match omp_profile(omp_profile_flag, &get) {
                Ok(None) => value("PI_CODING_AGENT_DIR").map(|dir| dir.join("sessions")),
                _ => None,
            },
        }),
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
            environment_session_dir_with(ManagedHarness::Claude, None, None, get).as_deref(),
            Some(std::path::Path::new("/stores/claude/projects"))
        );
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Codex, None, None, get).as_deref(),
            Some(std::path::Path::new("/stores/codex/sessions"))
        );
        assert_eq!(
            environment_session_dir_with(ManagedHarness::OpenCode, None, None, get).as_deref(),
            Some(std::path::Path::new("/stores/xdg/opencode"))
        );
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Omp, None, None, get).as_deref(),
            Some(std::path::Path::new("/stores/pi-family/sessions"))
        );
    }

    fn git_init(dir: &Path) {
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(dir)
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Only Crush's global config names: the global JSON files under the temp
    /// root, never the developer's.
    fn crush_env(root: &Path) -> impl Fn(&str) -> Option<OsString> + use<> {
        let config = root.join("global-config").into_os_string();
        let data = root.join("global-data").into_os_string();
        move |name| match name {
            "CRUSH_GLOBAL_CONFIG" => Some(config.clone()),
            "CRUSH_GLOBAL_DATA" => Some(data.clone()),
            _ => None,
        }
    }

    /// Without `--data-dir` Crush keeps `crush.db` in the closest `.crush`
    /// between the working directory and the git worktree root, not always
    /// in `<cwd>/.crush`; outside a worktree it looks only in the cwd.
    #[test]
    fn crush_data_dir_takes_the_closest_crush_dir_in_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let home = root.join("home");
        let repo = root.join("repo");
        let cwd = repo.join("sub").join("deep");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir(root.join(".crush")).unwrap();
        let resolve = |cwd: &Path| crush_data_dir(cwd, &home, crush_env(&root));

        // `<root>/.crush` is above the cwd but outside any worktree bound.
        assert_eq!(resolve(&cwd), cwd.join(".crush"));
        git_init(&repo);
        assert_eq!(
            resolve(&cwd),
            cwd.join(".crush"),
            "the walk stops at the worktree root"
        );
        std::fs::create_dir(repo.join(".crush")).unwrap();
        assert_eq!(resolve(&cwd), repo.join(".crush"));
        std::fs::create_dir(repo.join("sub").join(".crush")).unwrap();
        assert_eq!(resolve(&cwd), repo.join("sub").join(".crush"));
    }

    /// A `.crush` directly in the home is Crush's global state, never a
    /// project's data dir.
    #[test]
    fn crush_data_dir_skips_a_crush_dir_in_the_home() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let home = root.join("home");
        let cwd = home.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        git_init(&home);
        std::fs::create_dir(home.join(".crush")).unwrap();
        assert_eq!(
            crush_data_dir(&cwd, &home, crush_env(&root)),
            cwd.join(".crush")
        );
    }

    /// `options.data_directory` wins over the lookup. Later configs override
    /// earlier ones: the global JSON files, then the project's from the
    /// worktree root down, `.crush.json` over `crush.json` in one directory.
    /// A relative value is taken against the cwd and an empty one falls back
    /// to the lookup.
    #[test]
    fn crush_data_dir_follows_data_directory_in_crush_configs() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let home = root.join("home");
        let repo = root.join("repo");
        let cwd = repo.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        git_init(&repo);
        let write = |file: &Path, dir: &str| {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(
                file,
                serde_json::json!({"options": {"data_directory": dir}}).to_string(),
            )
            .unwrap();
        };
        let resolve = || crush_data_dir(&cwd, &home, crush_env(&root));

        write(
            &root.join("global-config").join("crush.json"),
            "/from/global",
        );
        assert_eq!(resolve(), Path::new("/from/global"));
        write(&root.join("global-data").join("crush.json"), "/from/data");
        assert_eq!(resolve(), Path::new("/from/data"));
        write(&repo.join(".crush.json"), "/from/repo-hidden");
        write(&repo.join("crush.json"), "/from/repo");
        assert_eq!(resolve(), Path::new("/from/repo-hidden"));
        write(&cwd.join("crush.json"), "state/../store");
        assert_eq!(resolve(), cwd.join("store"));
        write(&cwd.join("crush.json"), "");
        std::fs::create_dir(repo.join(".crush")).unwrap();
        assert_eq!(resolve(), repo.join(".crush"));
    }

    /// The launch plan carries the resolved Crush store when it knows where
    /// the launch runs; `--data-dir` still wins.
    #[test]
    fn crush_launch_plan_resolves_the_data_dir_from_the_launch_dir() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let repo = root.join("repo");
        let cwd = repo.join("sub");
        std::fs::create_dir_all(&cwd).unwrap();
        git_init(&repo);
        std::fs::create_dir(repo.join(".crush")).unwrap();
        let env = [
            (
                "CRUSH_GLOBAL_CONFIG".to_string(),
                root.join("global-config").display().to_string(),
            ),
            (
                "CRUSH_GLOBAL_DATA".to_string(),
                root.join("global-data").display().to_string(),
            ),
        ];
        let plan = |args: Vec<OsString>| {
            build_launch_plan_with_env(
                ManagedHarness::Crush,
                None,
                args,
                None,
                &env,
                Some(LaunchRoots {
                    home: &root,
                    cwd: &cwd,
                }),
            )
            .unwrap()
            .session_dir
        };
        assert_eq!(plan(Vec::new()), Some(repo.join(".crush")));
        assert_eq!(
            plan(vec![
                OsString::from("--data-dir"),
                OsString::from("/pinned")
            ]),
            Some(PathBuf::from("/pinned"))
        );
    }

    fn env_of(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let pairs = pairs.to_vec();
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn omp_profile_flag_reads_only_a_leading_profile() {
        let flag =
            |args: &[&str]| omp_profile_flag(&args.iter().map(OsString::from).collect::<Vec<_>>());
        assert_eq!(flag(&["--profile", "work"]).as_deref(), Some("work"));
        assert_eq!(
            flag(&["--profile=work", "--model", "x"]).as_deref(),
            Some("work")
        );
        assert_eq!(
            flag(&["launch", "--profile", "work"]).as_deref(),
            Some("work")
        );
        // OMP itself does not select a profile from any of these.
        assert_eq!(flag(&["grep", "--profile", "foo"]), None);
        assert_eq!(flag(&["--system-prompt", "--profile", "foo"]), None);
        assert_eq!(flag(&["--", "--profile", "foo"]), None);
        assert_eq!(flag(&["--profile", "--print"]), None);
        assert_eq!(flag(&["--profile="]), None);
        // Ambiguous without OMP's flag tables, so the environment decides.
        assert_eq!(flag(&["--model", "x", "--profile", "work"]), None);
        // OMP keeps the last `--profile`.
        assert_eq!(
            flag(&["--profile", "work", "--profile=personal"]).as_deref(),
            Some("personal")
        );
        assert_eq!(
            flag(&["--profile", "work", "--model", "x", "--profile", "b"]),
            None
        );
    }

    /// A `PI_CODING_AGENT_DIR` that a parent OMP derived for its profile is
    /// dropped once the default profile is back in charge, as OMP does; any
    /// other value is still honored.
    #[test]
    fn omp_default_profile_drops_a_profile_derived_agent_dir() {
        let home = Path::new("/home/me");
        let derived = "/home/me/.omp/profiles/work/agent";
        let default = home.join(".omp/agent");
        let dir = |explicit: Option<&str>, pairs: &[(&'static str, &'static str)]| {
            omp_agent_dir(home, explicit, env_of(pairs)).unwrap()
        };
        let inherited = [
            ("OMP_PROFILE", ""),
            ("PI_PROFILE", "work"),
            ("PI_CODING_AGENT_DIR", derived),
        ];
        assert_eq!(dir(None, &inherited), default);
        assert_eq!(
            dir(
                None,
                &[
                    ("OMP_PROFILE", ""),
                    ("PI_PROFILE", "work"),
                    ("PI_CONFIG_DIR", ".cfg"),
                    ("PI_CODING_AGENT_DIR", "/home/me/.cfg/profiles/work/agent")
                ]
            ),
            PathBuf::from("/home/me/.cfg/agent"),
            "the derived dir follows PI_CONFIG_DIR"
        );
        assert_eq!(
            dir(
                Some("default"),
                &[("OMP_PROFILE", "work"), ("PI_CODING_AGENT_DIR", derived)]
            ),
            default
        );
        assert_eq!(
            dir(
                None,
                &[
                    ("OMP_PROFILE", ""),
                    ("PI_PROFILE", "work"),
                    ("PI_CODING_AGENT_DIR", "/custom")
                ]
            ),
            PathBuf::from("/custom")
        );
        assert_eq!(
            dir(None, &[("PI_CODING_AGENT_DIR", derived)]),
            PathBuf::from(derived),
            "without an inherited profile the override is the user's own"
        );
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Omp, Some(home), None, env_of(&inherited)),
            None
        );
    }

    /// OMP keeps sessions in its agent dir: a named profile owns
    /// `~/.omp/profiles/<name>/agent` and ignores `PI_CODING_AGENT_DIR`,
    /// `--profile` beats `OMP_PROFILE`, and `PI_CODING_AGENT_SESSION_DIR`
    /// (OMP's `--session-dir` default) beats both.
    #[test]
    fn omp_session_dir_follows_profile_and_session_env() {
        let home = Path::new("/home/me");
        let profile = |name: &str| home.join(".omp/profiles").join(name).join("agent/sessions");
        let resolve = |flag: Option<&str>, pairs: &[(&'static str, &'static str)]| {
            environment_session_dir_with(ManagedHarness::Omp, Some(home), flag, env_of(pairs))
        };
        assert_eq!(
            resolve(None, &[("OMP_PROFILE", "work")]),
            Some(profile("work"))
        );
        assert_eq!(
            resolve(
                None,
                &[("OMP_PROFILE", "work"), ("PI_CODING_AGENT_DIR", "/x")]
            ),
            Some(profile("work"))
        );
        assert_eq!(
            resolve(None, &[("PI_PROFILE", "work")]),
            Some(profile("work"))
        );
        assert_eq!(
            resolve(Some("work"), &[("OMP_PROFILE", "other")]),
            Some(profile("work"))
        );
        assert_eq!(
            resolve(
                Some("default"),
                &[("OMP_PROFILE", "other"), ("PI_CODING_AGENT_DIR", "/x")]
            ),
            Some(PathBuf::from("/x/sessions"))
        );
        assert_eq!(
            resolve(
                None,
                &[
                    ("PI_CODING_AGENT_SESSION_DIR", "/s"),
                    ("OMP_PROFILE", "work")
                ]
            ),
            Some(PathBuf::from("/s"))
        );
        // No home, or a profile OMP refuses to start with: nothing to point at.
        assert_eq!(
            environment_session_dir_with(
                ManagedHarness::Omp,
                None,
                None,
                env_of(&[("OMP_PROFILE", "work")])
            ),
            None
        );
        assert_eq!(resolve(None, &[("OMP_PROFILE", "Work")]), None);

        let plan = build_launch_plan_with_env(
            ManagedHarness::Omp,
            None,
            vec![OsString::from("--profile"), OsString::from("work")],
            None,
            &[
                ("PI_CODING_AGENT_DIR".to_string(), "/x".to_string()),
                // Blank masks whatever the developer's shell exports.
                ("PI_CODING_AGENT_SESSION_DIR".to_string(), String::new()),
                ("PI_CONFIG_DIR".to_string(), String::new()),
                ("XDG_DATA_HOME".to_string(), String::new()),
            ],
            Some(LaunchRoots { home, cwd: home }),
        )
        .unwrap();
        assert_eq!(plan.session_dir, Some(profile("work")));
    }

    /// OMP joins `PI_CONFIG_DIR` under the home with Node's `path.join`: an
    /// absolute value stays under the home, `..` walks up, `~` is literal,
    /// and a blank value keeps `.omp`.
    #[test]
    fn omp_config_root_renames_like_node_path_join() {
        let home = Path::new("/home/me");
        for (value, expected) in [
            (".omp-alt", "/home/me/.omp-alt"),
            ("/abs/cfg", "/home/me/abs/cfg"),
            ("../sib", "/home/sib"),
            ("~/.x", "/home/me/~/.x"),
            ("./cfg/", "/home/me/cfg"),
            ("", "/home/me/.omp"),
            ("  ", "/home/me/.omp"),
        ] {
            let env = |name: &str| (name == "PI_CONFIG_DIR").then(|| OsString::from(value));
            assert_eq!(
                omp_config_root(home, env),
                PathBuf::from(expected),
                "{value:?}"
            );
            assert_eq!(
                omp_agent_dir(home, Some("work"), env).unwrap(),
                PathBuf::from(expected).join("profiles/work/agent"),
                "{value:?}"
            );
        }
        assert_eq!(omp_config_root(home, |_| None), home.join(".omp"));
        assert_eq!(
            environment_session_dir_with(
                ManagedHarness::Omp,
                Some(home),
                None,
                env_of(&[("PI_CONFIG_DIR", ".cfg")])
            ),
            Some(PathBuf::from("/home/me/.cfg/agent/sessions"))
        );
    }

    #[test]
    fn clean_path_folds_like_go_filepath_clean() {
        for (raw, expected) in [
            ("/a/b/../c", "/a/c"),
            ("/a/./b/", "/a/b"),
            ("/..", "/"),
            ("a/../..", ".."),
            ("", "."),
            ("./", "."),
        ] {
            assert_eq!(
                clean_path(Path::new(raw)),
                PathBuf::from(expected),
                "{raw:?}"
            );
        }
    }

    /// `omp --profile default` keeps the profile the environment named, so
    /// an agent dir inherited from that profile is still dropped.
    #[test]
    fn omp_profile_flag_env_keeps_the_inherited_profile_for_default() {
        let home = Path::new("/home/me");
        let resolve = |pairs: Vec<(String, String)>| {
            let inherited = env_of(&[
                ("OMP_PROFILE", "work"),
                ("PI_CODING_AGENT_DIR", "/home/me/.omp/profiles/work/agent"),
            ]);
            let merged = move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| OsString::from(value))
                    .or_else(|| inherited(name))
            };
            omp_agent_dir(home, None, merged).unwrap()
        };
        assert_eq!(
            resolve(omp_profile_flag_env(
                "default",
                env_of(&[("OMP_PROFILE", "work")])
            )),
            home.join(".omp/agent")
        );
        assert_eq!(
            resolve(omp_profile_flag_env("other", |_| None)),
            home.join(".omp/profiles/other/agent")
        );
        assert_eq!(
            omp_profile_flag_env("Bad", |_| None),
            vec![("OMP_PROFILE".to_string(), "Bad".to_string())],
            "an invalid name is passed on for the resolver to refuse"
        );
    }

    /// OMP resolves `PI_CODING_AGENT_DIR` before deciding whether it moved the
    /// agent dir, so `..` or a relative spelling of the default still counts
    /// as the default location and keeps XDG sessions.
    #[test]
    fn omp_xdg_sessions_compare_the_resolved_agent_dir() {
        let exists = |path: &Path| path == Path::new("/xdg/omp");
        let sessions = |pairs: &[(&'static str, &'static str)], home: &Path| {
            omp_sessions_dir(home, None, env_of(pairs), true, exists).unwrap()
        };
        let home = Path::new("/home/me");
        assert_eq!(
            sessions(
                &[
                    ("XDG_DATA_HOME", "/xdg"),
                    ("PI_CODING_AGENT_DIR", "/home/me/.omp/../.omp/agent")
                ],
                home
            ),
            PathBuf::from("/xdg/omp/sessions")
        );
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(
            sessions(
                &[
                    ("XDG_DATA_HOME", "/xdg"),
                    ("PI_CODING_AGENT_DIR", ".omp/agent")
                ],
                &cwd
            ),
            PathBuf::from("/xdg/omp/sessions"),
            "a relative override resolves against the working directory"
        );
    }

    /// Node's `path.win32.join` keeps a drive or UNC prefix of `PI_CONFIG_DIR`
    /// as plain segments under the home.
    #[cfg(windows)]
    #[test]
    fn omp_config_root_keeps_windows_prefixes_like_node() {
        let home = Path::new(r"C:\Users\me");
        for (value, expected) in [
            (r"\\server\share\omp", r"C:\Users\me\server\share\omp"),
            (r"D:\cfg", r"C:\Users\me\D:\cfg"),
        ] {
            let env = |name: &str| (name == "PI_CONFIG_DIR").then(|| OsString::from(value));
            assert_eq!(
                omp_config_root(home, env),
                PathBuf::from(expected),
                "{value}"
            );
        }
    }

    /// On Linux and macOS, OMP moves sessions (only sessions) under
    /// `$XDG_DATA_HOME/omp` once that directory exists, unless
    /// `PI_CODING_AGENT_DIR` moved the agent dir away from its default.
    #[test]
    fn omp_sessions_dir_follows_xdg_data_home() {
        let home = Path::new("/home/me");
        let existing = ["/xdg/omp", "/xdg2/omp/profiles/work", "/xdg3/omp"];
        let exists = |path: &Path| existing.iter().any(|dir| path == Path::new(dir));
        let sessions =
            |explicit: Option<&str>, pairs: &[(&'static str, &'static str)], xdg_platform: bool| {
                omp_sessions_dir(home, explicit, env_of(pairs), xdg_platform, exists).unwrap()
            };
        let xdg = [("XDG_DATA_HOME", "/xdg")];
        assert_eq!(
            sessions(None, &xdg, true),
            PathBuf::from("/xdg/omp/sessions")
        );
        assert_eq!(
            sessions(None, &xdg, false),
            home.join(".omp/agent/sessions"),
            "Windows keeps the agent dir"
        );
        assert_eq!(
            sessions(None, &[("XDG_DATA_HOME", "/missing")], true),
            home.join(".omp/agent/sessions")
        );
        assert_eq!(
            sessions(None, &[], true),
            home.join(".omp/agent/sessions"),
            "no ~/.local/share fallback"
        );
        assert_eq!(
            sessions(Some("work"), &xdg, true),
            home.join(".omp/profiles/work/agent/sessions"),
            "a profile keys on its own XDG dir, not the app root"
        );
        assert_eq!(
            sessions(Some("work"), &[("XDG_DATA_HOME", "/xdg2")], true),
            PathBuf::from("/xdg2/omp/profiles/work/sessions")
        );
        assert_eq!(
            sessions(
                None,
                &[
                    ("XDG_DATA_HOME", "/xdg"),
                    ("PI_CODING_AGENT_DIR", "/custom")
                ],
                true
            ),
            PathBuf::from("/custom/sessions"),
            "a relocated agent dir keeps its sessions"
        );
        assert_eq!(
            sessions(
                None,
                &[
                    ("XDG_DATA_HOME", "/xdg"),
                    ("PI_CODING_AGENT_DIR", "/home/me/.omp/agent/")
                ],
                true
            ),
            PathBuf::from("/xdg/omp/sessions"),
            "an override naming the default location is not a relocation"
        );
        assert_eq!(
            sessions(
                None,
                &[("XDG_DATA_HOME", "/xdg3"), ("PI_CONFIG_DIR", ".cfg")],
                true
            ),
            PathBuf::from("/xdg3/omp/sessions"),
            "PI_CONFIG_DIR does not rename the XDG app dir"
        );
        let probed = std::cell::Cell::new(false);
        let _ = omp_sessions_dir(
            home,
            None,
            env_of(&[("XDG_DATA_HOME", "  ")]),
            true,
            |_: &Path| {
                probed.set(true);
                true
            },
        );
        assert!(!probed.get(), "a blank XDG_DATA_HOME is unset");
    }

    /// The same rule through the launch plan, against the real filesystem.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn omp_session_import_reads_an_existing_xdg_data_dir() {
        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        let mut env = vec![(
            "XDG_DATA_HOME".to_string(),
            xdg.path().to_string_lossy().into_owned(),
        )];
        // Blank masks whatever the developer's shell exports.
        for name in [
            "PI_CODING_AGENT_SESSION_DIR",
            "PI_CODING_AGENT_DIR",
            "PI_CONFIG_DIR",
            "OMP_PROFILE",
        ] {
            env.push((name.to_string(), String::new()));
        }
        let plan = || {
            build_launch_plan_with_env(
                ManagedHarness::Omp,
                None,
                Vec::new(),
                None,
                &env,
                Some(LaunchRoots {
                    home: home.path(),
                    cwd: home.path(),
                }),
            )
            .unwrap()
            .session_dir
        };
        assert_eq!(plan(), None, "no XDG omp dir yet: the default store");
        std::fs::create_dir(xdg.path().join("omp")).unwrap();
        assert_eq!(plan(), Some(xdg.path().join("omp").join("sessions")));
    }

    /// Blank relocation values are unset, the same rule the hook and MCP
    /// installers apply; otherwise import read `<cwd>/   /...` while auto-wire
    /// installed into the default home.
    #[test]
    fn native_store_environment_overrides_treat_blank_as_unset() {
        for blank in ["", "   ", "\t"] {
            for harness in [
                ManagedHarness::Claude,
                ManagedHarness::Codex,
                ManagedHarness::OpenCode,
                ManagedHarness::OpenCode2,
                ManagedHarness::Pi,
                ManagedHarness::Omp,
                ManagedHarness::Kimi,
                ManagedHarness::Kiro,
                ManagedHarness::KiroV3,
                ManagedHarness::Grok,
            ] {
                assert_eq!(
                    environment_session_dir_with(
                        harness,
                        Some(Path::new("/home/me")),
                        None,
                        |_| Some(OsString::from(blank))
                    ),
                    None,
                    "{harness:?} with {blank:?}"
                );
            }
        }
        assert_eq!(
            environment_session_dir_with(
                ManagedHarness::Pi,
                None,
                None,
                env_of(&[
                    ("PI_CODING_AGENT_SESSION_DIR", "   "),
                    ("PI_CODING_AGENT_DIR", "/stores/pi")
                ])
            ),
            Some(PathBuf::from("/stores/pi/sessions"))
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
            environment_session_dir_with(ManagedHarness::Grok, None, None, get).as_deref(),
            Some(std::path::Path::new("/stores/grok/sessions"))
        );
    }

    #[test]
    fn kimi_home_environment_override_points_at_sessions_root() {
        let get =
            |name: &str| (name == "KIMI_CODE_HOME").then(|| OsString::from("/stores/kimi-code"));
        assert_eq!(
            environment_session_dir_with(ManagedHarness::Kimi, None, None, get).as_deref(),
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
            environment_session_dir_with(ManagedHarness::Kiro, None, None, get).as_deref(),
            Some(std::path::Path::new("/stores/kiro/sessions/cli"))
        );
        let get = |name: &str| (name == "KIRO_HOME").then(|| OsString::from("/stores/kiro"));
        assert_eq!(
            environment_session_dir_with(ManagedHarness::KiroV3, None, None, get).as_deref(),
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
