//! ai-jail's wrapper integration contract (ai-jail 3.0.1 and newer).
//!
//! Vendor doc: <https://github.com/akitaonrails/ai-jail/blob/v3.0.1/docs/WRAPPER_INTEGRATION.md>.
//! ai-memory names the harness it launches (`--harness`) so ai-jail neither
//! guesses it from ai-memory's argv nor reviews it as an unknown command, and
//! asks the read-only `status --json` whether the launch can start at all.
//!
//! What the session needs is passed as command-line flags (`--agent-state`,
//! `--env NAME`), which ai-jail applies without a prompt. The contract's
//! request manifest is not used: ai-jail asks a separate yes/no for every
//! request, one per variable, and the flags carry the same names with no
//! prompt. ai-jail's own first-run review stays the one place the user decides
//! the rest (network, credentials, SSH).
//!
//! Everything here is pure so each case is unit-tested without a sandbox.

use std::ffi::OsString;
use std::path::Path;

use serde::Deserialize;

use crate::ManagedHarness;
use crate::jail::JailSupport;

/// The installed ai-jail implements the wrapper contract this module speaks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailContract {
    /// ai-jail's version, from `capabilities --json`.
    pub version: String,
}

#[derive(Deserialize)]
struct Capabilities {
    version: String,
    schema_version: u32,
    #[serde(default)]
    flags: Vec<String>,
    features: CapabilityFeatures,
}

#[derive(Deserialize)]
struct CapabilityFeatures {
    #[serde(default)]
    harness_flag: bool,
    #[serde(default)]
    status_json: bool,
}

/// Read `ai-jail capabilities --json`. `None` unless it is schema version 1
/// with `--harness` and `status --json`; anything less keeps the older
/// flag-only launch.
#[must_use]
pub fn parse_capabilities(json: &str) -> Option<JailContract> {
    let capabilities: Capabilities = serde_json::from_str(json).ok()?;
    let features = &capabilities.features;
    (capabilities.schema_version == 1
        && features.harness_flag
        && features.status_json
        && capabilities.flags.iter().any(|flag| flag == "--harness"))
    .then_some(JailContract {
        version: capabilities.version,
    })
}

/// Ask the installed ai-jail for its wrapper contract. An older ai-jail treats
/// any unknown word as the command to sandbox, so `capabilities` is only asked
/// of one whose `--help` already lists `--harness`.
#[must_use]
pub fn ai_jail_contract(ai_jail: &Path, support: &JailSupport) -> Option<JailContract> {
    if !support.supports("harness") {
        return None;
    }
    let output = std::process::Command::new(ai_jail)
        .args(["capabilities", "--json"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_capabilities(&String::from_utf8_lossy(&output.stdout))
}

/// The name ai-jail knows `harness` by, for `--harness`. `None` for a harness
/// ai-jail has no preset for; those keep the flag-only launch, because
/// ai-jail refuses an unknown `--harness` name.
#[must_use]
pub const fn ai_jail_harness_name(harness: ManagedHarness) -> Option<&'static str> {
    match harness {
        ManagedHarness::Claude => Some("claude"),
        ManagedHarness::Codex => Some("codex"),
        ManagedHarness::OpenCode | ManagedHarness::OpenCode2 => Some("opencode"),
        ManagedHarness::Pi => Some("pi"),
        ManagedHarness::Crush => Some("crush"),
        ManagedHarness::Omp => Some("omp"),
        ManagedHarness::Kimi => Some("kimi"),
        ManagedHarness::Kiro | ManagedHarness::KiroV3 => Some("kiro-cli"),
        ManagedHarness::Grok => Some("grok"),
        ManagedHarness::Antigravity => Some("antigravity"),
        ManagedHarness::CommandCode | ManagedHarness::Copilot => None,
    }
}

/// Every provider credential in [`crate::FORWARDED_ENV_NAMES`].
const PROVIDER_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "COPILOT_GITHUB_TOKEN",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "OPENAI_API_KEY",
    "OPENROUTER_API_KEY",
];

/// The provider credentials `harness` itself reads, or `None` for a
/// multi-provider harness that may use any of them.
const fn harness_keys(harness: ManagedHarness) -> Option<&'static [&'static str]> {
    match harness {
        ManagedHarness::Claude => Some(&[
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_OAUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
        ]),
        ManagedHarness::Codex => Some(&["OPENAI_API_KEY"]),
        ManagedHarness::Antigravity => Some(&["GEMINI_API_KEY", "GOOGLE_API_KEY"]),
        ManagedHarness::Copilot => Some(&["COPILOT_GITHUB_TOKEN"]),
        // Their keys are not in FORWARDED_ENV_NAMES; ai-jail forwards
        // XAI_API_KEY and MOONSHOT_API_KEY itself with the harness state.
        ManagedHarness::Grok | ManagedHarness::Kimi => Some(&[]),
        ManagedHarness::OpenCode
        | ManagedHarness::OpenCode2
        | ManagedHarness::Pi
        | ManagedHarness::Crush
        | ManagedHarness::Omp
        | ManagedHarness::Kiro
        | ManagedHarness::KiroV3
        | ManagedHarness::CommandCode => None,
    }
}

/// The forwarded variable names that `harness` needs, from those set: every
/// ai-memory routing name, `CLAUDE_CONFIG_DIR` for Claude only, and only the
/// harness's own provider keys. A Claude session has no use for an OpenAI key,
/// so it never enters that jail.
#[must_use]
pub fn harness_env_names(harness: ManagedHarness, present: &[&'static str]) -> Vec<&'static str> {
    present
        .iter()
        .copied()
        .filter(|name| {
            if *name == "CLAUDE_CONFIG_DIR" {
                return harness == ManagedHarness::Claude;
            }
            if !PROVIDER_KEYS.contains(name) {
                return true;
            }
            harness_keys(harness).is_none_or(|keys| keys.contains(name))
        })
        .collect()
}

/// How the jailed harness reaches the memory server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerReach {
    /// The server listens on this host's IPv4 loopback. ai-jail's filtered
    /// network blocks loopback, so `--forward-port` exposes exactly that port
    /// on the jail's own loopback and nothing else of the host.
    Loopback {
        /// The server's port.
        port: u16,
    },
    /// Anywhere else. ai-jail's filter reaches a LAN or remote service only
    /// through its HTTP proxy, which ai-memory's hook clients deliberately
    /// bypass, so the launch keeps full network (`--network`, labelled OPEN in
    /// ai-jail's review).
    Network,
}

/// Classify the server URL. Only `localhost` and `127.0.0.0/8` count as
/// loopback: `--forward-port` bridges the host's `127.0.0.1`, so an IPv6
/// `[::1]` server is not reachable through it.
#[must_use]
pub fn server_reach(url: &str) -> ServerReach {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return ServerReach::Network;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    if host_port.starts_with('[') {
        return ServerReach::Network;
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(port) => (host, port),
            Err(_) => return ServerReach::Network,
        },
        None => match scheme.to_ascii_lowercase().as_str() {
            "http" => (host_port, 80),
            "https" => (host_port, 443),
            _ => return ServerReach::Network,
        },
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::Ipv4Addr>()
            .is_ok_and(|ip| ip.is_loopback());
    if loopback && port != 0 {
        ServerReach::Loopback { port }
    } else {
        ServerReach::Network
    }
}

/// The sandbox flags of a contract launch, without the command: the declared
/// harness, how the server is reached, the harness state, `--no-save-config`,
/// one `--env NAME` per variable, the user's own toggles and the extra
/// folders. All are command-line grants, which ai-jail applies without a
/// prompt.
///
/// With `--agent-state` ai-jail also forwards the declared harness's own API
/// key. `--no-save-config` stops ai-jail from saving these flags as permanent
/// project restrictions (an unchecked `--no-ssh` would otherwise keep SSH off
/// after the user approved it in ai-jail's review); the review's own approvals
/// are saved either way.
#[must_use]
pub fn contract_flags(
    harness: &str,
    reach: ServerReach,
    env_names: &[&str],
    toggles: &[crate::JailToggleChoice],
    folders: &[(&Path, bool)],
) -> Vec<OsString> {
    let mut argv = vec![OsString::from("--harness"), OsString::from(harness)];
    match reach {
        ServerReach::Loopback { port } => {
            argv.push(OsString::from("--forward-port"));
            argv.push(OsString::from(port.to_string()));
        }
        ServerReach::Network => argv.push(OsString::from("--network")),
    }
    argv.push(OsString::from("--agent-state"));
    argv.push(OsString::from("--no-save-config"));
    for name in env_names {
        argv.push(OsString::from("--env"));
        argv.push(OsString::from(*name));
    }
    argv.extend(toggles.iter().map(|toggle| OsString::from(toggle.flag())));
    for (path, writable) in folders {
        argv.push(OsString::from(if *writable { "--rw-map" } else { "--map" }));
        argv.push(path.as_os_str().to_os_string());
    }
    argv
}

/// `flags`, `--`, then the wrapped command: the launch argv.
#[must_use]
pub fn contract_invocation(
    flags: &[OsString],
    exe: &Path,
    forwarded_args: &[OsString],
) -> Vec<OsString> {
    let mut argv = flags.to_vec();
    argv.push(OsString::from("--"));
    argv.push(exe.as_os_str().to_os_string());
    argv.extend(forwarded_args.iter().cloned());
    argv
}

/// `flags status --json -- command`: the read-only preflight of exactly the
/// launch [`contract_invocation`] would make.
#[must_use]
pub fn contract_status_invocation(
    flags: &[OsString],
    exe: &Path,
    forwarded_args: &[OsString],
) -> Vec<OsString> {
    let mut argv = flags.to_vec();
    argv.push(OsString::from("status"));
    argv.push(OsString::from("--json"));
    argv.push(OsString::from("--"));
    argv.push(exe.as_os_str().to_os_string());
    argv.extend(forwarded_args.iter().cloned());
    argv
}

/// The blockers `status --json` reported, as stable code and ai-jail's
/// message.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JailStatus {
    /// `(code, message)` per blocker.
    pub blockers: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct StatusJson {
    schema_version: u32,
    #[serde(default)]
    blockers: Vec<StatusBlocker>,
}

#[derive(Deserialize)]
struct StatusBlocker {
    code: String,
    #[serde(default)]
    message: String,
}

/// Parse `status --json`. `None` for anything but schema version 1.
#[must_use]
pub fn parse_status(json: &str) -> Option<JailStatus> {
    let status: StatusJson = serde_json::from_str(json).ok()?;
    (status.schema_version == 1).then(|| JailStatus {
        blockers: status
            .blockers
            .into_iter()
            .map(|blocker| (blocker.code, blocker.message))
            .collect(),
    })
}

/// Why a contract launch cannot start, with ai-jail's own message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JailRefusal {
    /// The sandbox backend is missing or untrusted.
    Backend(String),
    /// A saved approval no longer matches this host: a missing variable, a
    /// replaced folder, a changed SSH key.
    Drift(String),
}

/// Blocker codes for a saved approval that drifted from the host.
const DRIFT_BLOCKERS: &[&str] = &["missing_approved_env", "directory_drift", "changed_ssh_key"];

/// The refusal `status` calls for, if any. Only a missing backend and drifted
/// approvals stop the launch here: a first run is ai-jail's own review to
/// hold, and an unknown code is ai-jail's to explain when it refuses.
#[must_use]
pub fn status_refusal(status: &JailStatus) -> Option<JailRefusal> {
    status.blockers.iter().find_map(|(code, message)| {
        if code == "backend_unavailable" {
            Some(JailRefusal::Backend(message.clone()))
        } else if DRIFT_BLOCKERS.contains(&code.as_str()) {
            Some(JailRefusal::Drift(message.clone()))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JailToggleChoice;

    const CAPABILITIES_3_0_1: &str = r#"{
      "version": "3.0.1",
      "schema_version": 1,
      "flags": ["--harness", "--request-file", "--request", "--json"],
      "request_kinds": ["private_host", "host", "env", "secret", "agent_state", "map"],
      "features": {
        "harness_flag": true, "request_file": true, "status_json": true,
        "secret_private_http": true, "configure_requests": true, "inside_marker": false
      },
      "exit_codes": {"cancelled": 130, "required_grant": 3, "blocked": 4, "backend_unavailable": 5}
    }"#;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn capabilities_of_3_0_1_enable_the_contract() {
        assert_eq!(
            parse_capabilities(CAPABILITIES_3_0_1),
            Some(JailContract {
                version: "3.0.1".to_owned()
            })
        );
    }

    #[test]
    fn capabilities_missing_any_piece_keep_the_flag_launch() {
        for (from, to) in [
            (r#""schema_version": 1"#, r#""schema_version": 2"#),
            (r#""harness_flag": true"#, r#""harness_flag": false"#),
            (r#""status_json": true"#, r#""status_json": false"#),
            (r#""--harness", "#, ""),
        ] {
            let json = CAPABILITIES_3_0_1.replace(from, to);
            assert_ne!(json, CAPABILITIES_3_0_1, "{from}");
            assert_eq!(parse_capabilities(&json), None, "{from} -> {to}");
        }
        assert_eq!(parse_capabilities("not json"), None);
        assert_eq!(parse_capabilities(""), None);
    }

    #[test]
    fn an_older_ai_jail_is_never_asked_for_capabilities() {
        // 2.15 would sandbox a command named `capabilities`; the help check
        // stops before any process starts (the path does not exist).
        let help_2_15 = JailSupport::from_help("--network / --no-network\n--map <PATH>\n");
        assert_eq!(
            ai_jail_contract(Path::new("/nonexistent/ai-jail"), &help_2_15),
            None
        );
    }

    const ALL_HARNESSES: [ManagedHarness; 14] = [
        ManagedHarness::Claude,
        ManagedHarness::Codex,
        ManagedHarness::OpenCode,
        ManagedHarness::OpenCode2,
        ManagedHarness::Pi,
        ManagedHarness::Crush,
        ManagedHarness::Omp,
        ManagedHarness::Kimi,
        ManagedHarness::CommandCode,
        ManagedHarness::Kiro,
        ManagedHarness::KiroV3,
        ManagedHarness::Grok,
        ManagedHarness::Antigravity,
        ManagedHarness::Copilot,
    ];

    #[test]
    fn every_managed_harness_maps_to_an_ai_jail_preset_or_none() {
        // The names ai-jail 3.0.1 accepts for `--harness` (src/command.rs
        // `canonical_harness_name` and `KNOWN_API_AGENTS`).
        const ACCEPTED: &[&str] = &[
            "claude",
            "codex",
            "opencode",
            "pi",
            "omp",
            "antigravity",
            "crush",
            "grok",
            "kimi",
            "kiro-cli",
        ];
        for harness in ALL_HARNESSES {
            if let Some(name) = ai_jail_harness_name(harness) {
                assert!(ACCEPTED.contains(&name), "{harness:?} -> {name}");
            }
        }
        assert_eq!(ai_jail_harness_name(ManagedHarness::Copilot), None);
        assert_eq!(ai_jail_harness_name(ManagedHarness::CommandCode), None);
    }

    #[test]
    fn a_harness_gets_only_its_own_provider_keys() {
        let present = crate::FORWARDED_ENV_NAMES;
        let claude = harness_env_names(ManagedHarness::Claude, present);
        for key in [
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CONFIG_DIR",
        ] {
            assert!(claude.contains(&key), "{key}");
        }
        for key in [
            "OPENAI_API_KEY",
            "GEMINI_API_KEY",
            "GOOGLE_API_KEY",
            "OPENROUTER_API_KEY",
        ] {
            assert!(!claude.contains(&key), "claude got {key}");
        }
        let codex = harness_env_names(ManagedHarness::Codex, present);
        assert!(codex.contains(&"OPENAI_API_KEY"));
        assert!(!codex.contains(&"ANTHROPIC_API_KEY"));
        assert!(!codex.contains(&"CLAUDE_CONFIG_DIR"));
        let grok = harness_env_names(ManagedHarness::Grok, present);
        assert!(
            PROVIDER_KEYS.iter().all(|key| !grok.contains(key)),
            "{grok:?}"
        );
        // Multi-provider harnesses keep every key that is set.
        let opencode = harness_env_names(ManagedHarness::OpenCode, present);
        assert!(
            PROVIDER_KEYS.iter().all(|key| opencode.contains(key)),
            "{opencode:?}"
        );
        // ai-memory's own routing reaches every harness.
        for harness in ALL_HARNESSES {
            let names = harness_env_names(harness, present);
            for name in [
                "AI_MEMORY_SERVER_URL",
                "AI_MEMORY_AUTH_TOKEN",
                "AI_MEMORY_JAIL_NEXT_RUN",
            ] {
                assert!(names.contains(&name), "{harness:?} lost {name}");
            }
        }
        // Only names that are set are ever passed.
        assert_eq!(
            harness_env_names(ManagedHarness::Claude, &["AI_MEMORY_SERVER_URL"]),
            ["AI_MEMORY_SERVER_URL"]
        );
    }

    #[test]
    fn every_forwarded_name_is_either_routing_or_a_known_provider_key() {
        // A new provider key added to FORWARDED_ENV_NAMES must be classified
        // here, or it would reach every harness's jail.
        for name in crate::FORWARDED_ENV_NAMES {
            assert!(
                PROVIDER_KEYS.contains(name)
                    || name.starts_with("AI_MEMORY_")
                    || *name == "CLAUDE_CONFIG_DIR",
                "{name} is unclassified"
            );
        }
    }

    #[test]
    fn server_reach_forwards_only_ipv4_loopback() {
        for (url, reach) in [
            (
                "http://127.0.0.1:49374",
                ServerReach::Loopback { port: 49374 },
            ),
            (
                "http://localhost:49374/base",
                ServerReach::Loopback { port: 49374 },
            ),
            (
                "http://127.0.0.2:8080",
                ServerReach::Loopback { port: 8080 },
            ),
            ("http://LOCALHOST", ServerReach::Loopback { port: 80 }),
            (
                "https://user:pw@localhost",
                ServerReach::Loopback { port: 443 },
            ),
            ("http://[::1]:49374", ServerReach::Network),
            ("http://192.168.0.90:49374", ServerReach::Network),
            ("https://memory.example.com", ServerReach::Network),
            ("http://localhost:notaport", ServerReach::Network),
            ("localhost:49374", ServerReach::Network),
            ("", ServerReach::Network),
        ] {
            assert_eq!(server_reach(url), reach, "{url}");
        }
    }

    #[test]
    fn contract_flags_are_grants_without_any_request() {
        let toggles = [JailToggleChoice {
            stem: "github",
            enable: true,
            implied: false,
        }];
        let flags = contract_flags(
            "claude",
            ServerReach::Loopback { port: 49374 },
            &["AI_MEMORY_SERVER_URL", "ANTHROPIC_API_KEY"],
            &toggles,
            &[(Path::new("/src/sibling"), false)],
        );
        assert_eq!(
            strings(&flags),
            [
                "--harness",
                "claude",
                "--forward-port",
                "49374",
                "--agent-state",
                "--no-save-config",
                "--env",
                "AI_MEMORY_SERVER_URL",
                "--env",
                "ANTHROPIC_API_KEY",
                "--github",
                "--map",
                "/src/sibling",
            ]
        );
        assert!(
            !strings(&flags)
                .iter()
                .any(|flag| flag == "--request-file" || flag == "--request"),
            "every request is a separate prompt in ai-jail"
        );
        let lan = contract_flags("codex", ServerReach::Network, &[], &[], &[]);
        assert_eq!(
            strings(&lan),
            [
                "--harness",
                "codex",
                "--network",
                "--agent-state",
                "--no-save-config"
            ]
        );
    }

    #[test]
    fn status_and_launch_share_the_flags_and_the_command() {
        let flags = vec![OsString::from("--harness"), OsString::from("claude")];
        let args = vec![OsString::from("run"), OsString::from("claude")];
        let exe = Path::new("/bin/ai-memory");
        assert_eq!(
            strings(&contract_invocation(&flags, exe, &args)),
            [
                "--harness",
                "claude",
                "--",
                "/bin/ai-memory",
                "run",
                "claude"
            ]
        );
        assert_eq!(
            strings(&contract_status_invocation(&flags, exe, &args)),
            [
                "--harness",
                "claude",
                "status",
                "--json",
                "--",
                "/bin/ai-memory",
                "run",
                "claude"
            ]
        );
    }

    /// The status 3.0.1 printed for a fresh project.
    const STATUS_FRESH: &str = r#"{
      "version": "3.0.1", "schema_version": 1,
      "project": {"canonical_path": "/p", "profile_present": false},
      "harnesses": [{"name": "claude", "onboarding_complete": false}],
      "grants": {"restricted": ["unrestricted_network"], "allowed": [], "open": []},
      "requests": {"approved": [], "pending": []},
      "blockers": [
        {"code": "onboarding_required", "message": "Interactive onboarding is required before launch"}
      ]
    }"#;

    #[test]
    fn a_first_run_is_left_to_ai_jails_own_review() {
        let status = parse_status(STATUS_FRESH).unwrap();
        assert_eq!(status.blockers.len(), 1);
        assert_eq!(status_refusal(&status), None);
    }

    #[test]
    fn only_a_missing_backend_or_drifted_access_refuses() {
        let with = |code: &str| JailStatus {
            blockers: vec![(code.to_owned(), "why".to_owned())],
        };
        for code in DRIFT_BLOCKERS {
            assert_eq!(
                status_refusal(&with(code)),
                Some(JailRefusal::Drift("why".to_owned())),
                "{code}"
            );
        }
        assert_eq!(
            status_refusal(&with("backend_unavailable")),
            Some(JailRefusal::Backend("why".to_owned()))
        );
        for code in [
            "onboarding_required",
            "required_grant_not_approved",
            "future_code",
        ] {
            assert_eq!(status_refusal(&with(code)), None, "{code}");
        }
        assert_eq!(status_refusal(&JailStatus::default()), None);
    }

    #[test]
    fn status_rejects_another_schema_and_garbage() {
        assert_eq!(
            parse_status(&STATUS_FRESH.replace(r#""schema_version": 1"#, r#""schema_version": 2"#)),
            None
        );
        assert_eq!(parse_status("{}"), None);
        assert_eq!(parse_status("nope"), None);
    }
}
