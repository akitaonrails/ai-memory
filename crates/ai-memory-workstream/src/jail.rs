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
    let candidate = usable_ai_jail(current_jail_os(), |name| match find_on_path(name) {
        Some(path) => Some(path),
        None if name == "bwrap" => std::env::var_os("BWRAP_BIN").map(PathBuf::from),
        None if name == "ai-jail" => home_local_bin_ai_jail(),
        None => None,
    })?;
    preflighted_ai_jail(candidate, ai_jail_preflight)
}

fn preflighted_ai_jail(
    candidate: PathBuf,
    preflight: impl FnOnce(&Path) -> bool,
) -> Option<PathBuf> {
    preflight(&candidate).then_some(candidate)
}

/// Ask the installed ai-jail to validate its complete local launch boundary.
/// `--dry-run` does not execute the child or write configuration, but it does
/// apply ai-jail's own backend trust checks (including root ownership and the
/// protected-Nix-store exception) so our offer cannot lead directly to a
/// refusal that the shallower PATH probe missed.
fn ai_jail_preflight(ai_jail: &Path) -> bool {
    let Ok(current_exe) = std::env::current_exe() else {
        return false;
    };
    std::process::Command::new(ai_jail)
        .args(["--dry-run", "--"])
        .arg(current_exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
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

/// Whether a toggle mounts a credential the agent can then use, or grants a
/// host capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JailToggleKind {
    /// Mounts secrets (read-only) into the jail.
    Credential,
    /// Exposes a host device, socket, or directory.
    Capability,
}

/// How a toggle's checklist visibility and smart default are computed
/// (docs/design-yolo-safety-ai-jail.md §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToggleRule {
    /// Shown, and pre-checked, only when this path under `$HOME` exists —
    /// the exact path ai-jail mounts, so a checked box always mounts something.
    CredentialAt(&'static str),
    /// Shown when `~/.ssh` exists or an agent socket is set; pre-checked only
    /// for an SSH-style `origin`, the one case `git push` needs it.
    Ssh,
    /// Shown unchecked; `linux_only` hides it where ai-jail ignores it.
    OptIn { linux_only: bool },
    /// Shown, pre-checked, only in a linked worktree: without its metadata
    /// mounted writable the agent cannot commit there.
    LinkedWorktree,
    /// Accepted from `--jail=…`, never shown in the checklist.
    CliOnly,
}

/// One ai-jail `--<stem>` / `--no-<stem>` toggle ai-memory can pass through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JailToggle {
    /// The ai-jail flag without its dashes, e.g. `docker-config`.
    pub stem: &'static str,
    /// Short checklist label.
    pub label: &'static str,
    /// One-line checklist note.
    pub note: &'static str,
    /// Credential or capability.
    pub kind: JailToggleKind,
    /// An ai-jail release known to accept the flag, named when the installed
    /// one does not.
    pub since: &'static str,
    rule: ToggleRule,
}

impl JailToggle {
    /// Whether the interactive checklist can show this toggle at all.
    #[must_use]
    pub const fn in_checklist(&self) -> bool {
        !matches!(self.rule, ToggleRule::CliOnly)
    }
}

/// First ai-jail release with the credential mounts and `--toolchains`.
const CREDENTIAL_MOUNTS_SINCE: &str = "2.5.0";
/// Oldest release this table was verified against; the remaining toggles all
/// exist there.
const BASE_TOGGLES_SINCE: &str = "2.4.1";

const fn toggle(
    stem: &'static str,
    label: &'static str,
    note: &'static str,
    kind: JailToggleKind,
    since: &'static str,
    rule: ToggleRule,
) -> JailToggle {
    JailToggle {
        stem,
        label,
        note,
        kind,
        since,
        rule,
    }
}

/// Every ai-jail toggle `ai-memory run --jail` accepts, in checklist order.
pub const JAIL_TOGGLES: &[JailToggle] = {
    use JailToggleKind::{Capability, Credential};
    use ToggleRule::{CliOnly, CredentialAt, LinkedWorktree, OptIn, Ssh};
    const CRED: &str = CREDENTIAL_MOUNTS_SINCE;
    const BASE: &str = BASE_TOGGLES_SINCE;
    &[
        toggle(
            "github",
            "GitHub CLI credentials",
            "~/.config/gh, read-only",
            Credential,
            CRED,
            CredentialAt(".config/gh"),
        ),
        toggle(
            "aws",
            "AWS credentials",
            "~/.aws, read-only",
            Credential,
            CRED,
            CredentialAt(".aws"),
        ),
        toggle(
            "kube",
            "Kubernetes credentials",
            "~/.kube, read-only",
            Credential,
            CRED,
            CredentialAt(".kube"),
        ),
        toggle(
            "gcloud",
            "Google Cloud credentials",
            "~/.config/gcloud, read-only",
            Credential,
            CRED,
            CredentialAt(".config/gcloud"),
        ),
        toggle(
            "docker-config",
            "Docker registry credentials",
            "~/.docker/config.json, read-only",
            Credential,
            CRED,
            CredentialAt(".docker/config.json"),
        ),
        toggle(
            "ssh",
            "SSH keys + agent",
            "~/.ssh read-only + SSH_AUTH_SOCK; needed for git over SSH",
            Credential,
            BASE,
            Ssh,
        ),
        toggle(
            "worktree",
            "Linked worktree metadata",
            "writable, so the agent can commit here",
            Capability,
            BASE,
            LinkedWorktree,
        ),
        toggle(
            "docker",
            "Docker socket",
            "⚠ grants host root",
            Capability,
            BASE,
            OptIn { linux_only: false },
        ),
        toggle(
            "gpu",
            "GPU devices",
            "/dev/dri, /dev/nvidia*",
            Capability,
            BASE,
            OptIn { linux_only: true },
        ),
        toggle(
            "display",
            "X11/Wayland display",
            "GUI apps and the clipboard",
            Capability,
            BASE,
            OptIn { linux_only: true },
        ),
        toggle(
            "pictures",
            "Pictures folder",
            "~/Pictures, read-only",
            Capability,
            BASE,
            OptIn { linux_only: false },
        ),
        toggle(
            "tailscale",
            "Tailscale socket",
            "reach your tailnet",
            Capability,
            BASE,
            OptIn { linux_only: false },
        ),
        toggle("audio", "Host audio", "", Capability, BASE, CliOnly),
        toggle("x11", "X11 socket", "", Capability, BASE, CliOnly),
        toggle(
            "host-shm",
            "Host shared memory",
            "",
            Capability,
            BASE,
            CliOnly,
        ),
        toggle(
            "terminal-passthrough",
            "Terminal passthrough",
            "",
            Capability,
            BASE,
            CliOnly,
        ),
        toggle(
            "update-check",
            "Update check",
            "",
            Capability,
            BASE,
            CliOnly,
        ),
        toggle("mise", "mise integration", "", Capability, BASE, CliOnly),
        toggle(
            "toolchains",
            "Toolchain caches",
            "",
            Capability,
            CRED,
            CliOnly,
        ),
    ]
};

/// The table entry for `stem`.
#[must_use]
pub fn jail_toggle(stem: &str) -> Option<&'static JailToggle> {
    JAIL_TOGGLES.iter().find(|toggle| toggle.stem == stem)
}

/// ai-jail switches `--jail=…` refuses: each weakens the sandbox itself
/// (`private-home`, `lockdown`, `landlock`, `seccomp`, `rlimits`,
/// `systemd-user`, `inherit-env`, `macos-host-ipc`, `browser`, the audit
/// log) or is set by ai-memory (`network`, `agent-state`, `save-config`). Whoever needs one
/// runs ai-jail directly.
const RESERVED_JAIL_FLAGS: &[&str] = &[
    "private-home",
    "lockdown",
    "landlock",
    "seccomp",
    "rlimits",
    "systemd-user",
    "inherit-env",
    "macos-host-ipc",
    "network",
    "agent-state",
    "browser",
    "audit-log",
    "audit-show",
    "audit-verify",
    "save-config",
];

/// The flags the installed ai-jail advertises in its `--help`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JailSupport {
    flags: std::collections::BTreeSet<String>,
}

impl JailSupport {
    /// Collect every `--flag` token from ai-jail's help text. Tokens split on
    /// whitespace and `/` (ai-jail prints `--ssh / --no-ssh`) and compare
    /// exactly, so `--docker-config` never counts as `--docker` or back.
    #[must_use]
    pub fn from_help(help: &str) -> Self {
        let flags = help
            .split(|c: char| c.is_whitespace() || c == '/')
            .filter(|token| token.starts_with("--") && token.len() > 2)
            .map(str::to_owned)
            .collect();
        Self { flags }
    }

    /// Whether the installed ai-jail accepts `--<stem>`.
    #[must_use]
    pub fn supports(&self, stem: &str) -> bool {
        self.flags.contains(&format!("--{stem}"))
    }
}

/// Run `<ai_jail> --help` once and read which toggles it accepts. An older
/// ai-jail rejects an unknown flag outright, so ai-memory only ever passes
/// flags listed here.
pub fn ai_jail_support(ai_jail: &Path) -> std::io::Result<JailSupport> {
    let output = std::process::Command::new(ai_jail)
        .arg("--help")
        .stdin(std::process::Stdio::null())
        .output()?;
    let mut help = String::from_utf8_lossy(&output.stdout).into_owned();
    help.push('\n');
    help.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(JailSupport::from_help(&help))
}

/// Host facts the toggle defaults read, injected so they are unit-testable
/// without the real home directory, environment, or repository.
#[derive(Debug, Clone)]
pub struct JailHostFacts {
    /// Host OS; Linux-only toggles are hidden elsewhere.
    pub os: JailOs,
    /// `$HOME` as ai-jail sees it (the credential mounts are relative to it).
    pub home: Option<PathBuf>,
    /// Whether `SSH_AUTH_SOCK` is set.
    pub ssh_agent: bool,
    /// The URL `git push` to `origin` uses.
    pub origin_push_url: Option<String>,
    /// Whether the cwd is a linked git worktree.
    pub linked_worktree: bool,
    /// Whether the invocation directory holds a project `.ai-jail`. ai-jail
    /// loads that file itself, under its own trust rules (a project file may
    /// disable capabilities but never enable credentials), so its presence
    /// alone hands the toggles to it: nothing is pre-checked and the
    /// interactive checklist is skipped. Only presence is probed — the
    /// contents are untrusted and never read, so a repository cannot steer
    /// the defaults through it.
    pub project_config: bool,
}

impl JailHostFacts {
    /// Facts for this host and the given repository. The project-config probe
    /// reads the process's own working directory, which the re-exec inherits
    /// and is where ai-jail looks; the global `~/.ai-jail` is deliberately
    /// ignored (ai-jail writes it for its own status-bar preferences).
    #[must_use]
    pub fn here(repository: &crate::RepositoryIdentity) -> Self {
        Self {
            project_config: std::env::current_dir().is_ok_and(|dir| dir.join(".ai-jail").is_file()),
            os: current_jail_os(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            ssh_agent: std::env::var_os("SSH_AUTH_SOCK").is_some_and(|sock| !sock.is_empty()),
            origin_push_url: repository.origin_push_url.clone(),
            linked_worktree: repository.linked_worktree,
        }
    }

    fn home_has(&self, relative: &str) -> bool {
        self.home
            .as_deref()
            .is_some_and(|home| home.join(relative).exists())
    }
}

/// Whether `url` reaches its remote over SSH: `ssh://…` (and the `git+ssh`
/// spellings) or git's scp-style `[user@]host:path` (a `:` before any `/`),
/// where `host` may be an `~/.ssh/config` alias. A one-letter `C:` is a drive.
#[must_use]
fn is_ssh_remote(url: &str) -> bool {
    let url = url.trim();
    if let Some((scheme, _)) = url.split_once("://") {
        return matches!(
            scheme.to_ascii_lowercase().as_str(),
            "ssh" | "git+ssh" | "ssh+git"
        );
    }
    url.split_once(':').is_some_and(|(host, _)| {
        let drive = host.len() == 1 && host.as_bytes()[0].is_ascii_alphabetic();
        !host.is_empty() && !drive && !host.contains(['/', '\\'])
    })
}

/// One visible checklist row with its smart default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JailChecklistItem {
    /// The toggle shown.
    pub toggle: &'static JailToggle,
    /// Pre-checked state: what pressing Enter enables.
    pub checked: bool,
}

/// The checklist rows for this host: toggles the installed ai-jail supports,
/// that apply on this OS and, for credentials, exist on this host — each with
/// its smart default (docs/design-yolo-safety-ai-jail.md §5).
#[must_use]
pub fn jail_checklist(facts: &JailHostFacts, support: &JailSupport) -> Vec<JailChecklistItem> {
    JAIL_TOGGLES
        .iter()
        .filter(|toggle| support.supports(toggle.stem))
        .filter_map(|toggle| {
            let (visible, checked) = match toggle.rule {
                ToggleRule::CredentialAt(path) => {
                    let present = facts.home_has(path);
                    (present, present)
                }
                ToggleRule::Ssh => (
                    facts.home_has(".ssh") || facts.ssh_agent,
                    facts.origin_push_url.as_deref().is_some_and(is_ssh_remote),
                ),
                ToggleRule::OptIn { linux_only } => {
                    (!linux_only || facts.os == JailOs::Linux, false)
                }
                ToggleRule::LinkedWorktree => (facts.linked_worktree, true),
                ToggleRule::CliOnly => (false, false),
            };
            visible.then_some(JailChecklistItem {
                toggle,
                checked: checked && !facts.project_config,
            })
        })
        .collect()
}

/// One toggle ai-memory passes to ai-jail: `--<stem>` or `--no-<stem>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JailToggleChoice {
    /// The toggle's stem.
    pub stem: &'static str,
    /// `true` emits `--<stem>`, `false` emits `--no-<stem>`.
    pub enable: bool,
    /// Off only because an explicit selection left this visible row out
    /// (an unchecked checklist row, or a row a `--jail=…` list did not
    /// name), as opposed to a `no-X` the user typed. The flag is the same;
    /// the summary line groups these as "everything else in the checklist".
    pub implied: bool,
}

impl JailToggleChoice {
    /// The ai-jail flag this choice emits.
    #[must_use]
    pub fn flag(&self) -> String {
        if self.enable {
            format!("--{}", self.stem)
        } else {
            format!("--no-{}", self.stem)
        }
    }
}

/// The checked rows as choices: what a bare `--jail` enables. The user saw no
/// selection there, so unchecked rows emit nothing and their own ai-jail
/// configuration (e.g. a global `~/.ai-jail`) still applies to the rest.
#[must_use]
pub fn checked_choices(items: &[JailChecklistItem]) -> Vec<JailToggleChoice> {
    items
        .iter()
        .filter(|item| item.checked)
        .map(|item| JailToggleChoice {
            stem: item.toggle.stem,
            enable: true,
            implied: false,
        })
        .collect()
}

/// Every row's state as the user saw it, for the interactive checklist:
/// checked emits `--X`, unchecked `--no-X`. The checklist is
/// what-you-see-is-what-you-get — an unchecked `[ ] Docker socket` stays off
/// even when the user's global `~/.ai-jail` enables it.
#[must_use]
pub fn marked_choices(items: &[JailChecklistItem]) -> Vec<JailToggleChoice> {
    items
        .iter()
        .map(|item| JailToggleChoice {
            stem: item.toggle.stem,
            enable: item.checked,
            implied: !item.checked,
        })
        .collect()
}

/// Why a `--jail=…` toggle list was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JailToggleError {
    /// Not a toggle ai-memory knows.
    Unknown(String),
    /// A security switch or ai-memory-owned flag (see [`RESERVED_JAIL_FLAGS`]).
    Reserved(String),
    /// A known toggle the installed ai-jail does not accept.
    Unsupported {
        /// The toggle's stem.
        stem: &'static str,
        /// A release known to accept it.
        since: &'static str,
    },
}

impl std::fmt::Display for JailToggleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown(name) => {
                let valid: Vec<&str> = JAIL_TOGGLES.iter().map(|toggle| toggle.stem).collect();
                write!(
                    f,
                    "--jail: unknown toggle `{name}`; valid toggles: {}, plus `all`, `none`, and a `no-` prefix to force one off",
                    valid.join(", ")
                )
            }
            Self::Reserved(name) => write!(
                f,
                "--jail: `{name}` is not available through ai-memory; run ai-jail directly"
            ),
            Self::Unsupported { stem, since } => write!(
                f,
                "--jail: the installed ai-jail does not support --{stem}; it needs ai-jail {since} or newer"
            ),
        }
    }
}

impl std::error::Error for JailToggleError {}

/// Parse a `--jail=…` list: comma-separated stems, each optionally prefixed
/// `no-` to force it off, plus `all` (every row of `checklist`) and `none`
/// (clear everything listed so far). Later entries win, so `all,no-docker`
/// works.
///
/// An empty list means a bare `--jail`: the smart defaults only
/// ([`checked_choices`]), deferring to the user's ai-jail config for the
/// rest. Any other list is an explicit selection and exact: after the named
/// entries (in order), every visible `checklist` row the list did not name is
/// forced off with `--no-X`, so nothing else is enabled even when a global
/// `~/.ai-jail` would, and `none` turns every checklist row off. Rows not in
/// `checklist` (absent credentials, CLI-only toggles) are never forced: they
/// stay out of the invocation unless named.
pub fn parse_jail_toggles(
    spec: &str,
    checklist: &[JailChecklistItem],
    support: &JailSupport,
) -> Result<Vec<JailToggleChoice>, JailToggleError> {
    let items: Vec<&str> = spec
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .collect();
    if items.is_empty() {
        return Ok(checked_choices(checklist));
    }
    let mut choices: Vec<JailToggleChoice> = Vec::new();
    fn set(choices: &mut Vec<JailToggleChoice>, choice: JailToggleChoice) {
        choices.retain(|existing| existing.stem != choice.stem);
        choices.push(choice);
    }
    for item in items {
        let lowered = item.to_ascii_lowercase();
        match lowered.as_str() {
            "all" => {
                for row in checklist {
                    set(
                        &mut choices,
                        JailToggleChoice {
                            stem: row.toggle.stem,
                            enable: true,
                            implied: false,
                        },
                    );
                }
                continue;
            }
            "none" => {
                choices.clear();
                continue;
            }
            _ => {}
        }
        let (stem, enable) = match lowered.strip_prefix("no-") {
            Some(stem) => (stem, false),
            None => (lowered.as_str(), true),
        };
        if RESERVED_JAIL_FLAGS.contains(&stem) {
            return Err(JailToggleError::Reserved(item.to_owned()));
        }
        let Some(toggle) = jail_toggle(stem) else {
            return Err(JailToggleError::Unknown(item.to_owned()));
        };
        if !support.supports(toggle.stem) {
            return Err(JailToggleError::Unsupported {
                stem: toggle.stem,
                since: toggle.since,
            });
        }
        set(
            &mut choices,
            JailToggleChoice {
                stem: toggle.stem,
                enable,
                implied: false,
            },
        );
    }
    for row in checklist {
        if !choices.iter().any(|choice| choice.stem == row.toggle.stem) {
            choices.push(JailToggleChoice {
                stem: row.toggle.stem,
                enable: false,
                implied: true,
            });
        }
    }
    Ok(choices)
}

/// Build the argument vector for `ai-jail` (excluding the `ai-jail` program
/// name itself): `--network`, an optional bare `--agent-state` toggle, an
/// optional `--no-save-config`, one `--env NAME` per already-filtered present
/// name, one `--<stem>` /
/// `--no-<stem>` per chosen toggle, a `--` separator, then the wrapped
/// executable and its forwarded arguments in order.
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
///
/// `no_save_config` must be set whenever the installed ai-jail supports it:
/// on every ordinary run ai-jail otherwise merges its CLI flags into the
/// project `.ai-jail` and writes it back, so the first jailed run would
/// persist ai-memory's transient flags (`--network`, `--agent-state`, the
/// chosen credentials) into the user's repository. Every later run would then
/// find a project file, skip the checklist, and lose the credential mounts a
/// project file cannot enable. `toggles` must
/// already be limited to flags the installed ai-jail accepts
/// ([`parse_jail_toggles`] / [`jail_checklist`]); they precede the `--` for
/// the same reason every other sandbox flag does.
#[must_use]
pub fn build_ai_jail_invocation(
    exe: &Path,
    forwarded_args: &[OsString],
    env_names_present: &[&str],
    agent_state: bool,
    no_save_config: bool,
    toggles: &[JailToggleChoice],
) -> Vec<OsString> {
    let mut argv = vec![OsString::from("--network")];
    if agent_state {
        argv.push(OsString::from("--agent-state"));
    }
    if no_save_config {
        argv.push(OsString::from("--no-save-config"));
    }
    for name in env_names_present {
        argv.push(OsString::from("--env"));
        argv.push(OsString::from(*name));
    }
    argv.extend(toggles.iter().map(|toggle| OsString::from(toggle.flag())));
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

    #[test]
    fn failed_ai_jail_preflight_suppresses_the_offer() {
        let candidate = PathBuf::from("/opt/bin/ai-jail");
        assert_eq!(
            preflighted_ai_jail(candidate.clone(), |path| {
                assert_eq!(path, candidate);
                false
            }),
            None
        );
        assert_eq!(
            preflighted_ai_jail(candidate.clone(), |_| true),
            Some(candidate)
        );
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
        let argv = build_ai_jail_invocation(exe, &forwarded, &present, true, true, &[]);
        assert_eq!(
            strings(&argv),
            [
                "--network",
                "--agent-state",
                "--no-save-config",
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
        let argv = build_ai_jail_invocation(exe, &[], &[], false, false, &[]);
        assert_eq!(
            strings(&argv),
            ["--network", "--", "/usr/local/bin/ai-memory"]
        );
    }

    #[test]
    fn build_ai_jail_invocation_only_forwards_present_env_names() {
        let exe = Path::new("/bin/ai-memory");
        let argv = build_ai_jail_invocation(exe, &[], &["CLAUDE_CONFIG_DIR"], false, false, &[]);
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
        let argv = strings(&build_ai_jail_invocation(
            exe,
            &forwarded,
            &[],
            true,
            true,
            &[],
        ));
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
        assert_eq!(
            &argv[..separator],
            ["--network", "--agent-state", "--no-save-config"]
        );
    }

    /// The toggle lines of ai-jail 2.4.1's real `--help` (no credential
    /// mounts, no `--toolchains`).
    const HELP_2_4_1: &str = "\
    --private-home / --no-private-home
    --lockdown / --no-lockdown     Enable/disable strict read-only lockdown mode
    --save-config / --no-save-config
    --no-gpu / --gpu               Disable/enable GPU device passthrough (Linux only)
    --no-docker / --docker         Disable/enable Docker socket passthrough (grants host root; default: off)
    --tailscale / --no-tailscale   Enable/disable Tailscale socket passthrough (default: off)
    --no-display / --display       Disable/enable X11/Wayland passthrough (Linux only)
    --audio / --no-audio           Enable/disable host audio passthrough
    --network / --no-network       Enable/disable unrestricted network access (default: off)
    --x11 / --no-x11               Enable/disable X11 socket passthrough (default: off)
    --worktree / --no-worktree     Enable/disable linked Git worktree metadata passthrough
    --no-mise / --mise             Disable/enable mise integration
    --ssh / --no-ssh               Share ~/.ssh read-only + forward SSH_AUTH_SOCK (default: off)
    --pictures / --no-pictures     Share ~/Pictures read-only (default: off)
    --browser[=PROFILE]            Use browser isolation profile (hard | soft; default hard)
";

    /// ai-jail 2.5.0 adds these lines on top of 2.4.1's.
    const HELP_2_5_0_EXTRA: &str = "\
    --github / --no-github         Enable/disable read-only ~/.config/gh mount
    --aws / --no-aws               Enable/disable read-only ~/.aws mount
    --kube / --no-kube             Enable/disable read-only ~/.kube mount
    --gcloud / --no-gcloud         Enable/disable read-only ~/.config/gcloud
    --docker-config / --no-docker-config
    --no-toolchains / --toolchains Disable/enable dev-toolchain cache
";

    fn support_2_4_1() -> JailSupport {
        JailSupport::from_help(HELP_2_4_1)
    }

    fn support_2_5_0() -> JailSupport {
        JailSupport::from_help(&format!("{HELP_2_4_1}{HELP_2_5_0_EXTRA}"))
    }

    /// A temp `$HOME` holding exactly `paths` (relative), and no repo facts.
    fn facts_with(home: &Path, paths: &[&str]) -> JailHostFacts {
        for path in paths {
            let full = home.join(path);
            if path.ends_with(".json") {
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(&full, "{}").unwrap();
            } else {
                std::fs::create_dir_all(&full).unwrap();
            }
        }
        JailHostFacts {
            os: JailOs::Linux,
            home: Some(home.to_path_buf()),
            ssh_agent: false,
            origin_push_url: None,
            linked_worktree: false,
            project_config: false,
        }
    }

    fn rows(items: &[JailChecklistItem]) -> Vec<(&'static str, bool)> {
        items
            .iter()
            .map(|item| (item.toggle.stem, item.checked))
            .collect()
    }

    fn flags(choices: &[JailToggleChoice]) -> Vec<String> {
        choices.iter().map(JailToggleChoice::flag).collect()
    }

    #[test]
    fn toggle_table_has_unique_stems_and_never_lists_a_reserved_flag() {
        let mut stems: Vec<&str> = JAIL_TOGGLES.iter().map(|toggle| toggle.stem).collect();
        stems.sort_unstable();
        let before = stems.len();
        stems.dedup();
        assert_eq!(stems.len(), before, "duplicate toggle stem");
        for reserved in RESERVED_JAIL_FLAGS {
            assert!(!stems.contains(reserved), "{reserved} must not be a toggle");
        }
        let cli_only: Vec<&str> = JAIL_TOGGLES
            .iter()
            .filter(|toggle| !toggle.in_checklist())
            .map(|toggle| toggle.stem)
            .collect();
        assert_eq!(
            cli_only,
            [
                "audio",
                "x11",
                "host-shm",
                "terminal-passthrough",
                "update-check",
                "mise",
                "toolchains"
            ]
        );
        let docker = JAIL_TOGGLES.iter().find(|t| t.stem == "docker").unwrap();
        assert!(docker.note.contains("grants host root"));
    }

    #[test]
    fn support_detection_reads_exact_tokens_from_help() {
        let old = support_2_4_1();
        assert!(old.supports("ssh") && old.supports("gpu") && old.supports("mise"));
        assert!(!old.supports("github"), "2.4.1 has no --github");
        assert!(!old.supports("toolchains"));
        assert!(old.supports("no-save-config"), "both spellings are tokens");
        assert!(!JailSupport::from_help("").supports("no-save-config"));
        let new = support_2_5_0();
        assert!(new.supports("github") && new.supports("docker-config"));
        assert!(
            new.supports("toolchains"),
            "`--no-toolchains / --toolchains`"
        );

        // `--docker` and `--docker-config` are distinct tokens either way.
        let only_config = JailSupport::from_help("    --docker-config / --no-docker-config\n");
        assert!(only_config.supports("docker-config"));
        assert!(!only_config.supports("docker"));
        let only_socket =
            JailSupport::from_help("    --no-docker / --docker   (grants host root)\n");
        assert!(only_socket.supports("docker"));
        assert!(!only_socket.supports("docker-config"));
        // A valued flag's metavar does not make the bare stem supported.
        assert!(!old.supports("browser"));
    }

    #[test]
    fn credentials_are_shown_and_checked_only_when_present() {
        let home = tempfile::tempdir().unwrap();
        let facts = facts_with(home.path(), &[".config/gh", ".aws", ".docker/config.json"]);
        let items = jail_checklist(&facts, &support_2_5_0());
        let credentials: Vec<_> = rows(&items)
            .into_iter()
            .filter(|(stem, _)| ["github", "aws", "kube", "gcloud", "docker-config"].contains(stem))
            .collect();
        assert_eq!(
            credentials,
            [("github", true), ("aws", true), ("docker-config", true)],
            "absent ~/.kube and ~/.config/gcloud are hidden"
        );
        // A bare `~/.docker` directory is not the registry config ai-jail mounts.
        let bare = tempfile::tempdir().unwrap();
        let facts = facts_with(bare.path(), &[".docker"]);
        assert!(
            !rows(&jail_checklist(&facts, &support_2_5_0()))
                .iter()
                .any(|(stem, _)| *stem == "docker-config")
        );
    }

    #[test]
    fn credentials_are_hidden_when_the_installed_ai_jail_lacks_them() {
        let home = tempfile::tempdir().unwrap();
        let facts = facts_with(home.path(), &[".config/gh", ".aws"]);
        let stems: Vec<_> = rows(&jail_checklist(&facts, &support_2_4_1()))
            .into_iter()
            .map(|(stem, _)| stem)
            .collect();
        assert!(!stems.contains(&"github") && !stems.contains(&"aws"));
        assert!(stems.contains(&"docker"), "2.4.1 toggles still show");
    }

    #[test]
    fn ssh_shows_with_keys_or_agent_and_defaults_on_only_for_ssh_remotes() {
        let empty = tempfile::tempdir().unwrap();
        let mut facts = facts_with(empty.path(), &[]);
        let ssh = |facts: &JailHostFacts| {
            rows(&jail_checklist(facts, &support_2_4_1()))
                .into_iter()
                .find(|(stem, _)| *stem == "ssh")
        };
        assert_eq!(ssh(&facts), None, "no ~/.ssh and no agent: hidden");
        facts.ssh_agent = true;
        assert_eq!(ssh(&facts), Some(("ssh", false)), "agent only, no remote");

        let keys = tempfile::tempdir().unwrap();
        let mut facts = facts_with(keys.path(), &[".ssh"]);
        for (url, expected) in [
            ("git@github.com:example/repo.git", true),
            ("ssh://git@example.com/repo.git", true),
            ("deploy@host.example:repo.git", true),
            ("work-gh:example/repo.git", true),
            ("[::1]:repo.git", true),
            ("https://github.com/example/repo.git", false),
            ("https://user@example.com/repo.git", false),
            ("/srv/git/repo.git", false),
            ("file:///srv/git/repo.git", false),
            ("./dir:with-colon/repo.git", false),
            ("C:/src/repo.git", false),
            (r"C:\src\repo.git", false),
        ] {
            facts.origin_push_url = Some(url.to_owned());
            assert_eq!(ssh(&facts), Some(("ssh", expected)), "{url}");
        }
    }

    #[test]
    fn worktree_shows_checked_only_in_a_linked_worktree() {
        let home = tempfile::tempdir().unwrap();
        let mut facts = facts_with(home.path(), &[]);
        let worktree = |facts: &JailHostFacts| {
            rows(&jail_checklist(facts, &support_2_4_1()))
                .into_iter()
                .find(|(stem, _)| *stem == "worktree")
        };
        assert_eq!(worktree(&facts), None);
        facts.linked_worktree = true;
        assert_eq!(worktree(&facts), Some(("worktree", true)));
    }

    #[test]
    fn capabilities_default_off_and_linux_only_ones_hide_elsewhere() {
        let home = tempfile::tempdir().unwrap();
        let mut facts = facts_with(home.path(), &[]);
        assert_eq!(
            rows(&jail_checklist(&facts, &support_2_4_1())),
            [
                ("docker", false),
                ("gpu", false),
                ("display", false),
                ("pictures", false),
                ("tailscale", false)
            ]
        );
        facts.os = JailOs::MacOs;
        assert_eq!(
            rows(&jail_checklist(&facts, &support_2_4_1())),
            [("docker", false), ("pictures", false), ("tailscale", false)]
        );
    }

    /// A project `.ai-jail` hands the toggles to ai-jail: nothing is
    /// pre-checked (so a bare `--jail` passes none), while the rows stay known
    /// so an explicit `--jail=all` / `--jail=LIST` still overrides the file.
    #[test]
    fn project_config_unchecks_every_smart_default_but_keeps_explicit_lists() {
        let home = tempfile::tempdir().unwrap();
        let mut facts = facts_with(home.path(), &[".config/gh", ".aws", ".ssh"]);
        facts.origin_push_url = Some("git@github.com:example/repo.git".to_owned());
        facts.linked_worktree = true;
        let support = support_2_5_0();
        assert_eq!(
            flags(&checked_choices(&jail_checklist(&facts, &support))),
            ["--github", "--aws", "--ssh", "--worktree"],
            "control: without a project file the defaults are on"
        );

        facts.project_config = true;
        let checklist = jail_checklist(&facts, &support);
        assert!(!checklist.is_empty());
        assert!(checklist.iter().all(|item| !item.checked));
        let parse = |spec: &str| flags(&parse_jail_toggles(spec, &checklist, &support).unwrap());
        assert_eq!(
            parse(""),
            Vec::<String>::new(),
            "bare --jail defers to the file"
        );
        let listed = parse("github,gpu");
        assert_eq!(listed[..2], ["--github", "--gpu"]);
        assert!(
            listed[2..].iter().all(|flag| flag.starts_with("--no-")),
            "an explicit list forces the unnamed rows off even with a project file: {listed:?}"
        );
        assert_eq!(listed.len(), checklist.len());
        let all = parse("all");
        assert_eq!(all.len(), checklist.len());
        assert!(all.iter().all(|flag| !flag.starts_with("--no-")));
    }

    #[test]
    fn save_config_is_owned_by_ai_memory() {
        let error = parse_jail_toggles("save-config", &[], &support_2_5_0()).unwrap_err();
        assert_eq!(error, JailToggleError::Reserved("save-config".to_owned()));
        let error = parse_jail_toggles("no-save-config", &[], &support_2_5_0()).unwrap_err();
        assert_eq!(
            error,
            JailToggleError::Reserved("no-save-config".to_owned())
        );
    }

    fn sample_checklist(home: &Path) -> Vec<JailChecklistItem> {
        let mut facts = facts_with(home, &[".config/gh", ".aws", ".ssh"]);
        facts.origin_push_url = Some("https://example.com/repo.git".to_owned());
        jail_checklist(&facts, &support_2_5_0())
    }

    #[test]
    fn toggle_list_parses_stems_negations_all_and_none() {
        let home = tempfile::tempdir().unwrap();
        let checklist = sample_checklist(home.path());
        let support = support_2_5_0();
        let parse = |spec: &str| flags(&parse_jail_toggles(spec, &checklist, &support).unwrap());
        // The visible rows of `sample_checklist`, in order.
        let rows = [
            "github",
            "aws",
            "ssh",
            "docker",
            "gpu",
            "display",
            "pictures",
            "tailscale",
        ];
        // The named flags, then `--no-X` for every visible row not named.
        let exact = |named: &[&str]| -> Vec<String> {
            let mut expected: Vec<String> = named.iter().map(|flag| (*flag).to_owned()).collect();
            for row in rows {
                let mentioned = named
                    .iter()
                    .any(|flag| *flag == format!("--{row}") || *flag == format!("--no-{row}"));
                if !mentioned {
                    expected.push(format!("--no-{row}"));
                }
            }
            expected
        };

        assert_eq!(
            parse(""),
            ["--github", "--aws"],
            "empty = smart defaults only, nothing forced"
        );
        assert_eq!(parse("gpu, ssh"), exact(&["--gpu", "--ssh"]));
        assert_eq!(
            parse("github,aws,no-mise"),
            exact(&["--github", "--aws", "--no-mise"])
        );
        assert_eq!(parse("GPU"), exact(&["--gpu"]), "case-insensitive");
        assert_eq!(parse("gpu,no-gpu"), exact(&["--no-gpu"]), "later wins");
        assert_eq!(
            parse("all"),
            rows.map(|row| format!("--{row}")),
            "all = every visible checklist row, nothing left to force"
        );
        assert_eq!(
            parse("all,no-docker"),
            [
                "--github",
                "--aws",
                "--ssh",
                "--gpu",
                "--display",
                "--pictures",
                "--tailscale",
                "--no-docker"
            ]
        );
        assert_eq!(parse("none"), exact(&[]), "none = every visible row off");
        assert_eq!(parse("all,none,toolchains"), exact(&["--toolchains"]));
        assert_eq!(
            parse("kube"),
            exact(&["--kube"]),
            "an absent credential may still be named; it is never forced"
        );
        assert!(
            !parse("gpu")
                .iter()
                .any(|flag| flag.contains("mise") || flag.contains("kube")),
            "rows outside the checklist are never forced off"
        );
    }

    /// Adversarial: an explicit selection must be exact even when the user's
    /// global `~/.ai-jail` enables something. An unchecked Docker row and
    /// `--jail=none` must both emit `--no-docker` (and `none`, a `--no-X` for
    /// every visible row) — emitting nothing would let that config mount the
    /// host-root Docker socket behind an unchecked box.
    #[test]
    fn explicit_selections_force_every_unselected_visible_row_off() {
        let home = tempfile::tempdir().unwrap();
        let checklist = sample_checklist(home.path());
        let docker = checklist
            .iter()
            .find(|item| item.toggle.stem == "docker")
            .expect("docker row visible");
        assert!(!docker.checked, "docker is unchecked by default");
        let marked = flags(&marked_choices(&checklist));
        assert_eq!(
            marked,
            [
                "--github",
                "--aws",
                "--no-ssh",
                "--no-docker",
                "--no-gpu",
                "--no-display",
                "--no-pictures",
                "--no-tailscale"
            ],
            "the checklist emits every row as the user saw it"
        );
        assert!(
            !flags(&checked_choices(&checklist)).contains(&"--no-docker".to_owned()),
            "control: bare --jail (no selection shown) defers the rest to ai-jail config"
        );

        let none = flags(&parse_jail_toggles("none", &checklist, &support_2_5_0()).unwrap());
        assert_eq!(none.len(), checklist.len());
        for item in &checklist {
            assert!(
                none.contains(&format!("--no-{}", item.toggle.stem)),
                "--jail=none must force {} off: {none:?}",
                item.toggle.stem
            );
        }
    }

    #[test]
    fn toggle_list_rejects_unknown_and_reserved_names() {
        let home = tempfile::tempdir().unwrap();
        let checklist = sample_checklist(home.path());
        let support = support_2_5_0();
        let unknown = parse_jail_toggles("gpu,bogus", &checklist, &support).unwrap_err();
        assert_eq!(unknown, JailToggleError::Unknown("bogus".to_owned()));
        let message = unknown.to_string();
        assert!(message.contains("bogus") && message.contains("docker-config"));
        assert!(message.contains("github") && message.contains("all"));

        for reserved in [
            "seccomp",
            "no-seccomp",
            "landlock",
            "private-home",
            "no-private-home",
            "lockdown",
            "rlimits",
            "systemd-user",
            "inherit-env",
            "macos-host-ipc",
            "network",
            "no-network",
            "agent-state",
            "browser",
            "audit-log",
        ] {
            let error = parse_jail_toggles(reserved, &checklist, &support).unwrap_err();
            assert_eq!(error, JailToggleError::Reserved(reserved.to_owned()));
            assert!(
                error
                    .to_string()
                    .contains("not available through ai-memory; run ai-jail directly"),
                "{error}"
            );
        }
    }

    #[test]
    fn toggle_list_names_the_ai_jail_release_a_missing_flag_needs() {
        let home = tempfile::tempdir().unwrap();
        let facts = facts_with(home.path(), &[".config/gh", ".ssh"]);
        let checklist = jail_checklist(&facts, &support_2_4_1());
        let error = parse_jail_toggles("ssh,github", &checklist, &support_2_4_1()).unwrap_err();
        assert_eq!(
            error,
            JailToggleError::Unsupported {
                stem: "github",
                since: "2.5.0"
            }
        );
        assert!(error.to_string().contains("ai-jail 2.5.0 or newer"));
        assert!(parse_jail_toggles("no-toolchains", &checklist, &support_2_4_1()).is_err());
        assert_eq!(
            flags(&parse_jail_toggles("ssh,gpu", &checklist, &support_2_4_1()).unwrap()),
            [
                "--ssh",
                "--gpu",
                "--no-docker",
                "--no-display",
                "--no-pictures",
                "--no-tailscale"
            ],
            "2.4.1 has no github row to force"
        );
    }

    #[test]
    fn build_ai_jail_invocation_places_toggles_between_the_baseline_and_separator() {
        let exe = Path::new("/bin/ai-memory");
        let toggles = [
            JailToggleChoice {
                stem: "gpu",
                enable: true,
                implied: false,
            },
            JailToggleChoice {
                stem: "mise",
                enable: false,
                implied: false,
            },
        ];
        let forwarded = ["run", "claude", "--jail=gpu,no-mise"].map(OsString::from);
        let argv = strings(&build_ai_jail_invocation(
            exe,
            &forwarded,
            &["AI_MEMORY_SERVER_URL"],
            true,
            true,
            &toggles,
        ));
        assert_eq!(
            argv,
            [
                "--network",
                "--agent-state",
                "--no-save-config",
                "--env",
                "AI_MEMORY_SERVER_URL",
                "--gpu",
                "--no-mise",
                "--",
                "/bin/ai-memory",
                "run",
                "claude",
                "--jail=gpu,no-mise",
            ]
        );
    }
}
