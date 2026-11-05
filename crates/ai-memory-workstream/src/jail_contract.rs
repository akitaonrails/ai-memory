//! ai-jail's wrapper integration contract (ai-jail 3.0.1 and newer).
//!
//! Vendor doc: <https://github.com/akitaonrails/ai-jail/blob/v3.0.1/docs/WRAPPER_INTEGRATION.md>.
//! ai-memory declares the harness it launches (`--harness`) and describes what
//! it needs in a request manifest (`--request-file`). A request is never a
//! grant: ai-jail's own review shows it, the user approves or denies it there,
//! and ai-jail saves the approval outside the checkout. A read-only
//! `status --json` tells the wrapper, before it launches, whether a review is
//! still needed. Everything here is pure so each case of the doc is
//! unit-tested without a sandbox.

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
    #[serde(default)]
    request_kinds: Vec<String>,
    features: CapabilityFeatures,
}

#[derive(Deserialize)]
struct CapabilityFeatures {
    #[serde(default)]
    harness_flag: bool,
    #[serde(default)]
    request_file: bool,
    #[serde(default)]
    status_json: bool,
    #[serde(default)]
    configure_requests: bool,
}

/// Read `ai-jail capabilities --json`. `None` unless it is schema version 1
/// and offers every piece ai-memory relies on: `--harness`, `--request-file`,
/// `status --json`, `configure` with requests, and the `env` and
/// `agent_state` request kinds. Anything less keeps the older flag-based
/// launch.
#[must_use]
pub fn parse_capabilities(json: &str) -> Option<JailContract> {
    let capabilities: Capabilities = serde_json::from_str(json).ok()?;
    let features = &capabilities.features;
    let has_flag = |flag: &str| capabilities.flags.iter().any(|known| known == flag);
    let has_kind = |kind: &str| capabilities.request_kinds.iter().any(|known| known == kind);
    (capabilities.schema_version == 1
        && features.harness_flag
        && features.request_file
        && features.status_json
        && features.configure_requests
        && has_flag("--harness")
        && has_flag("--request-file")
        && has_kind("env")
        && has_kind("agent_state"))
    .then_some(JailContract {
        version: capabilities.version,
    })
}

/// Ask the installed ai-jail for its wrapper contract. An older ai-jail treats
/// any unknown word as the command to sandbox, so `capabilities` is only asked
/// of one whose `--help` already lists `--harness` and `--request-file`.
#[must_use]
pub fn ai_jail_contract(ai_jail: &Path, support: &JailSupport) -> Option<JailContract> {
    if !(support.supports("harness") && support.supports("request-file")) {
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
/// ai-jail has no preset for; those keep the flag-based launch, because
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

/// One entry of the request manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JailRequest {
    /// Pass this environment variable's name (ai-jail copies the value from
    /// its own environment; the manifest never holds it).
    Env(&'static str),
    /// Mount the harness's own login and state directories.
    AgentState,
}

/// Why ai-memory asks for `name`, shown in ai-jail's review.
fn env_reason(name: &str) -> &'static str {
    match name {
        "AI_MEMORY_SERVER_URL" | "AI_MEMORY_HOOK_URL" | "AI_MEMORY_BASE_PATH" => {
            "where ai-memory hooks send the session's activity"
        }
        "AI_MEMORY_AUTH_TOKEN" => "ai-memory server login for the hooks",
        "AI_MEMORY_DATA_DIR" => "ai-memory's data directory",
        "AI_MEMORY_JAIL_NEXT_RUN" => "the command ai-memory prints for the next launch",
        "CLAUDE_CONFIG_DIR" => "where Claude Code keeps its settings",
        _ => "the harness's own API login",
    }
}

/// The manifest ai-memory hands ai-jail: schema version 1, requester
/// `ai-memory`, one item per request with the reason the review shows. The
/// order is fixed (agent state, then `env_names` in the given order) so an
/// unchanged launch writes an identical file and ai-jail's exact approval
/// match keeps holding.
#[must_use]
pub fn request_manifest(requests: &[JailRequest]) -> String {
    let items: Vec<serde_json::Value> = requests
        .iter()
        .map(|request| match request {
            JailRequest::AgentState => serde_json::json!({
                "kind": "agent_state",
                "value": true,
                "reason": "the harness keeps its login between jailed runs",
            }),
            JailRequest::Env(name) => serde_json::json!({
                "kind": "env",
                "value": name,
                "reason": env_reason(name),
            }),
        })
        .collect();
    let manifest = serde_json::json!({
        "version": 1,
        "requester": "ai-memory",
        "requests": items,
    });
    let mut text = serde_json::to_string_pretty(&manifest).unwrap_or_default();
    text.push('\n');
    text
}

/// The requests for one launch: the harness's state, then each forwarded
/// environment name actually present.
#[must_use]
pub fn contract_requests(env_names_present: &[&'static str]) -> Vec<JailRequest> {
    std::iter::once(JailRequest::AgentState)
        .chain(env_names_present.iter().copied().map(JailRequest::Env))
        .collect()
}

/// The sandbox flags of a contract launch, without the command: the declared
/// harness, the manifest, how the server is reached, `--no-save-config`, the
/// user's own toggles and the extra folders. Toggles and folders stay flags:
/// the user chose them in ai-memory (`--jail=…`, the folder picker,
/// `--jail-map`), which is the consent a request would ask for again.
///
/// `--no-save-config` stops ai-jail from saving these flags as permanent
/// project restrictions (an unchecked `--no-ssh` would otherwise keep SSH off
/// after the user approved it in ai-jail's review). The review's own approvals
/// are saved either way.
#[must_use]
pub fn contract_flags(
    harness: &str,
    request_file: &Path,
    reach: ServerReach,
    toggles: &[crate::JailToggleChoice],
    folders: &[(&Path, bool)],
) -> Vec<OsString> {
    let mut argv = vec![
        OsString::from("--harness"),
        OsString::from(harness),
        OsString::from("--request-file"),
        request_file.as_os_str().to_os_string(),
    ];
    match reach {
        ServerReach::Loopback { port } => {
            argv.push(OsString::from("--forward-port"));
            argv.push(OsString::from(port.to_string()));
        }
        ServerReach::Network => argv.push(OsString::from("--network")),
    }
    argv.push(OsString::from("--no-save-config"));
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

/// `configure --harness H --request-file P`: ai-jail's review of the requests.
#[must_use]
pub fn contract_configure_invocation(harness: &str, request_file: &Path) -> Vec<OsString> {
    vec![
        OsString::from("configure"),
        OsString::from("--harness"),
        OsString::from(harness),
        OsString::from("--request-file"),
        request_file.as_os_str().to_os_string(),
    ]
}

/// What `status --json` reported that ai-memory acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JailStatus {
    /// Every declared harness finished ai-jail's first-run review here.
    pub onboarding_complete: bool,
    /// Requests ai-jail has not approved, as `kind value`.
    pub pending: Vec<String>,
    /// Stable blocker codes with ai-jail's message.
    pub blockers: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct StatusJson {
    schema_version: u32,
    #[serde(default)]
    harnesses: Vec<StatusHarness>,
    #[serde(default)]
    requests: StatusRequests,
    #[serde(default)]
    blockers: Vec<StatusBlocker>,
}

#[derive(Deserialize)]
struct StatusHarness {
    #[serde(default)]
    onboarding_complete: bool,
}

#[derive(Deserialize, Default)]
struct StatusRequests {
    #[serde(default)]
    pending: Vec<StatusRequest>,
}

#[derive(Deserialize)]
struct StatusRequest {
    kind: String,
    value: serde_json::Value,
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
    if status.schema_version != 1 {
        return None;
    }
    Some(JailStatus {
        onboarding_complete: !status.harnesses.is_empty()
            && status
                .harnesses
                .iter()
                .all(|harness| harness.onboarding_complete),
        pending: status
            .requests
            .pending
            .into_iter()
            .map(|request| {
                let value = match request.value {
                    serde_json::Value::String(text) => text,
                    other => other.to_string(),
                };
                format!("{} {value}", request.kind)
            })
            .collect(),
        blockers: status
            .blockers
            .into_iter()
            .map(|blocker| (blocker.code, blocker.message))
            .collect(),
    })
}

/// What to do before the launch, from [`JailStatus`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JailPreflight {
    /// Launch. On a first run in a terminal, ai-jail's own review covers the
    /// requests.
    Launch,
    /// Run ai-jail's review (`configure`) first: a reviewed project gained a
    /// request, and ai-jail refuses to launch until it is approved.
    Configure,
    /// The launch cannot succeed now; the reason says why and what to do.
    Refuse(JailRefusal),
}

/// Why a contract launch was refused before it started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JailRefusal {
    /// The sandbox backend is missing or untrusted (ai-jail's message).
    Backend(String),
    /// A saved approval no longer matches this host: a missing variable, a
    /// replaced folder, a changed SSH key (ai-jail's message).
    Drift(String),
    /// A review is needed and there is no terminal to hold it.
    NeedsReview,
}

/// Blocker codes for a saved approval that drifted from the host
/// (`status --json`, exit 4).
const DRIFT_BLOCKERS: &[&str] = &["missing_approved_env", "directory_drift", "changed_ssh_key"];

/// Decide the preflight. A backend or drift blocker refuses; pending requests
/// in a reviewed project need `configure`; a first run needs a terminal for
/// ai-jail's review. Unknown blocker codes are left to ai-jail, which states
/// them itself when it refuses.
#[must_use]
pub fn preflight(status: &JailStatus, interactive: bool) -> JailPreflight {
    for (code, message) in &status.blockers {
        if code == "backend_unavailable" {
            return JailPreflight::Refuse(JailRefusal::Backend(message.clone()));
        }
        if DRIFT_BLOCKERS.contains(&code.as_str()) {
            return JailPreflight::Refuse(JailRefusal::Drift(message.clone()));
        }
    }
    let review_needed = !status.onboarding_complete || !status.pending.is_empty();
    if !review_needed {
        return JailPreflight::Launch;
    }
    if !interactive {
        return JailPreflight::Refuse(JailRefusal::NeedsReview);
    }
    if status.onboarding_complete {
        JailPreflight::Configure
    } else {
        JailPreflight::Launch
    }
}

/// ai-jail's machine exit codes (`capabilities --json` `exit_codes`).
pub mod exit_code {
    /// A request is refused, changed, or not approved.
    pub const REQUIRED_GRANT: i32 = 3;
    /// A saved approval is blocked by host drift.
    pub const BLOCKED: i32 = 4;
    /// The platform sandbox backend is unavailable.
    pub const BACKEND_UNAVAILABLE: i32 = 5;
    /// The user cancelled the review.
    pub const CANCELLED: i32 = 130;
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
            (r#""request_file": true"#, r#""request_file": false"#),
            (r#""status_json": true"#, r#""status_json": false"#),
            (
                r#""configure_requests": true"#,
                r#""configure_requests": false"#,
            ),
            (r#""--harness", "#, ""),
            (r#""agent_state", "#, ""),
            (r#""env", "#, ""),
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
        for harness in [
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
        ] {
            if let Some(name) = ai_jail_harness_name(harness) {
                assert!(ACCEPTED.contains(&name), "{harness:?} -> {name}");
            }
        }
        assert_eq!(ai_jail_harness_name(ManagedHarness::Copilot), None);
        assert_eq!(ai_jail_harness_name(ManagedHarness::CommandCode), None);
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
    fn the_manifest_is_schema_1_with_reasons_and_no_values() {
        let manifest = request_manifest(&contract_requests(&[
            "AI_MEMORY_SERVER_URL",
            "AI_MEMORY_AUTH_TOKEN",
        ]));
        let parsed: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(parsed["version"], 1);
        assert_eq!(parsed["requester"], "ai-memory");
        let requests = parsed["requests"].as_array().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0]["kind"], "agent_state");
        assert_eq!(requests[0]["value"], true);
        assert_eq!(requests[1]["kind"], "env");
        assert_eq!(requests[1]["value"], "AI_MEMORY_SERVER_URL");
        assert_eq!(requests[2]["value"], "AI_MEMORY_AUTH_TOKEN");
        for request in requests {
            assert!(!request["reason"].as_str().unwrap().is_empty(), "{request}");
        }
        // Stable bytes: an unchanged launch writes the same manifest.
        assert_eq!(
            manifest,
            request_manifest(&contract_requests(&[
                "AI_MEMORY_SERVER_URL",
                "AI_MEMORY_AUTH_TOKEN"
            ]))
        );
    }

    #[test]
    fn contract_flags_declare_the_harness_and_reach_the_server() {
        let toggles = [JailToggleChoice {
            stem: "github",
            enable: true,
            implied: false,
        }];
        let flags = contract_flags(
            "claude",
            Path::new("/data/jail-requests/abc.json"),
            ServerReach::Loopback { port: 49374 },
            &toggles,
            &[(Path::new("/src/sibling"), false)],
        );
        assert_eq!(
            strings(&flags),
            [
                "--harness",
                "claude",
                "--request-file",
                "/data/jail-requests/abc.json",
                "--forward-port",
                "49374",
                "--no-save-config",
                "--github",
                "--map",
                "/src/sibling",
            ]
        );
        let lan = contract_flags(
            "codex",
            Path::new("/r.json"),
            ServerReach::Network,
            &[],
            &[],
        );
        assert_eq!(
            strings(&lan),
            [
                "--harness",
                "codex",
                "--request-file",
                "/r.json",
                "--network",
                "--no-save-config"
            ]
        );
        // No `--agent-state` and no `--env`: those are requests now.
        assert!(
            !strings(&flags)
                .iter()
                .any(|flag| flag == "--agent-state" || flag == "--env")
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
        assert_eq!(
            strings(&contract_configure_invocation(
                "claude",
                Path::new("/r.json")
            )),
            [
                "configure",
                "--harness",
                "claude",
                "--request-file",
                "/r.json"
            ]
        );
    }

    /// The status 3.0.1 printed for a fresh project with four requests.
    const STATUS_FRESH: &str = r#"{
      "version": "3.0.1", "schema_version": 1,
      "project": {"canonical_path": "/p", "profile_present": false},
      "harnesses": [{"name": "claude", "onboarding_complete": false}],
      "grants": {"restricted": ["unrestricted_network"], "allowed": [], "open": []},
      "requests": {"approved": [], "pending": [
        {"requester": "ai-memory", "kind": "private_host", "value": "192.168.0.90", "port": 49374},
        {"requester": "ai-memory", "kind": "agent_state", "value": "true"}
      ]},
      "blockers": [
        {"code": "onboarding_required", "message": "Interactive onboarding is required before launch"},
        {"code": "required_grant_not_approved", "message": "One or more wrapper requests are pending approval"}
      ]
    }"#;

    #[test]
    fn status_of_a_fresh_project_needs_the_first_run_review() {
        let status = parse_status(STATUS_FRESH).unwrap();
        assert!(!status.onboarding_complete);
        assert_eq!(
            status.pending,
            ["private_host 192.168.0.90", "agent_state true"]
        );
        assert_eq!(preflight(&status, true), JailPreflight::Launch);
        assert_eq!(
            preflight(&status, false),
            JailPreflight::Refuse(JailRefusal::NeedsReview)
        );
    }

    #[test]
    fn preflight_decision_table() {
        let reviewed = |pending: &[&str], blockers: &[(&str, &str)]| JailStatus {
            onboarding_complete: true,
            pending: pending.iter().map(|text| (*text).to_owned()).collect(),
            blockers: blockers
                .iter()
                .map(|(code, message)| ((*code).to_owned(), (*message).to_owned()))
                .collect(),
        };
        // Reviewed and nothing pending: launch, with or without a terminal.
        assert_eq!(preflight(&reviewed(&[], &[]), true), JailPreflight::Launch);
        assert_eq!(preflight(&reviewed(&[], &[]), false), JailPreflight::Launch);
        // A new request in a reviewed project: configure first, or refuse
        // without a terminal (ai-jail would exit 3 either way).
        let new = reviewed(&["env AI_MEMORY_AUTH_TOKEN"], &[]);
        assert_eq!(preflight(&new, true), JailPreflight::Configure);
        assert_eq!(
            preflight(&new, false),
            JailPreflight::Refuse(JailRefusal::NeedsReview)
        );
        // Drift and a missing backend refuse even in a terminal, quoting ai-jail.
        for code in DRIFT_BLOCKERS {
            assert_eq!(
                preflight(&reviewed(&[], &[(code, "why")]), true),
                JailPreflight::Refuse(JailRefusal::Drift("why".to_owned())),
                "{code}"
            );
        }
        assert_eq!(
            preflight(&reviewed(&[], &[("backend_unavailable", "no bwrap")]), true),
            JailPreflight::Refuse(JailRefusal::Backend("no bwrap".to_owned()))
        );
        // An unknown code is ai-jail's to explain.
        assert_eq!(
            preflight(&reviewed(&[], &[("future_code", "x")]), true),
            JailPreflight::Launch
        );
    }

    #[test]
    fn status_rejects_another_schema_and_garbage() {
        assert_eq!(
            parse_status(&STATUS_FRESH.replace(r#""schema_version": 1"#, r#""schema_version": 2"#)),
            None
        );
        assert_eq!(parse_status("{}"), None);
        assert_eq!(parse_status("nope"), None);
        // No harness listed is not a completed review.
        let none = parse_status(r#"{"schema_version": 1}"#).unwrap();
        assert!(!none.onboarding_complete);
    }
}
