//! `ai-memory setup` — a native install in one command.
//!
//! A composition of the commands a native install otherwise runs by hand, in
//! order, through their own library functions:
//!
//! 1. `init`, which on a fresh data dir also mints the root bearer token into
//!    `<data_dir>/auth-tokens/default` and records `[auth] bearer_token_file`;
//! 2. a per-user service running `serve` on that install
//!    ([`super::user_service`]): a systemd `--user` unit or a launchd
//!    LaunchAgent, rendered from the packaged templates, or on Windows a
//!    logon entry under the per-user `Run` key that starts
//!    [`super::service_host`];
//! 3. for each agent (named with `--agent`, else detected from its session
//!    store or MCP config under the home, the way `doctor` finds them):
//!    `install-mcp --apply`, `install-hooks --apply` and global
//!    `install-skills`;
//! 4. a self-check that writes nothing: `/healthz`, a `memory_status` call
//!    over MCP with the install's credential, and `doctor`.
//!
//! The token is never printed or passed on a command line. The installers keep
//! it out of every harness config (hooks read the credential files `init`
//! seeds; MCP clients go through `mcp-bridge` or an env-var reference), as
//! they do when run by hand.
//!
//! Every step is idempotent, so a second `setup` changes nothing. Without
//! `--yes` the plan is shown first and confirmed on a terminal, where the
//! operator can also pick which of the planned agents to wire; with no
//! terminal to ask on, `setup` changes nothing and exits non-zero. `--plan`
//! prints the plan (as JSON with `--json`, for a front end to render) and
//! changes nothing. `--web` shows the same plan as a local page and applies
//! the agents picked there through `setup --yes` ([`super::setup_web`]).

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ai_memory_workstream::{ManagedHarness, OpenCodeDialect, native_store_root};
use anyhow::{Context as _, Result, bail};

use super::user_service::{self, CommandRunner, ServiceChange, ServiceManager, ServiceUnit};
use super::{
    agent_plugins, doctor, init, install_hooks, install_mcp, install_skills, run_autowire,
};
use crate::cli::{
    AgentChoice, DoctorArgs, HookInstallScope, InitArgs, InstallHooksArgs, InstallMcpArgs,
    InstallSkillsAgent, InstallSkillsArgs, InstallSkillsScope, McpClient, SetupArgs,
};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, ServerProbe, probe_server};

/// How long `setup` waits for a service it just started to answer.
const SERVICE_READY_TIMEOUT: Duration = Duration::from_secs(30);

/// The MCP entry name every installer writes by default.
const ENTRY_NAME: &str = "ai-memory";

/// What `setup` tells the operator to ask a restarted agent: answering it
/// takes a call to ai-memory's tools, so the reply shows the wiring works.
const FIRST_QUESTION: &str = "What do you remember about this project?";

/// Bumped when a field of the `--plan --json` document changes meaning or
/// goes away; adding a field does not bump it.
const PLAN_FORMAT: u32 = 1;

/// Harnesses that share a native store with another one, so finding the store
/// cannot tell them apart: OpenCode 2 shares OpenCode's, and the Kiro CLI v3
/// engine's store contains the v2 engine's. Wiring both would write two
/// dialects into one config, so only the stable one is detected; the other is
/// wired when named with `--agent`. A detected OpenCode still gets the major
/// its version probe reports ([`opencode_dialect`]).
const EXPLICIT_ONLY: &[ManagedHarness] = &[ManagedHarness::OpenCode2, ManagedHarness::KiroV3];

/// What `setup` will do.
#[derive(Debug)]
struct SetupPlan {
    data_dir: PathBuf,
    config_path: PathBuf,
    /// No config file yet: `init` writes one (and mints the token when the
    /// data dir has no database either).
    fresh: bool,
    service: ServicePlan,
    agents: Vec<AgentPlan>,
    /// Agents came from detection, not `--agent`.
    detected: bool,
}

#[derive(Debug)]
enum ServicePlan {
    Install(ServiceUnit),
    Skip(String),
}

#[derive(Debug)]
enum SetupProbe {
    Healthy,
    Unreachable(String),
    Unexpected(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AgentPlan {
    agent: AgentChoice,
    /// Where detection found the agent; `None` when it was named with
    /// `--agent`.
    found: Option<Found>,
    mcp: Option<McpClient>,
    skills: InstallSkillsAgent,
    /// The ai-memory marketplace plugin is enabled for this agent: it carries
    /// the hooks, MCP entry and skills, so setup writes none of them (#1166).
    plugin: bool,
}

/// The evidence detection found an agent by, shown in the plan so the
/// operator can tell why an agent is listed before agreeing to wire it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
enum Found {
    SessionStore(PathBuf),
    McpConfig(PathBuf),
    ConfigDir(PathBuf),
}

impl Found {
    fn describe(&self) -> String {
        match self {
            Self::SessionStore(path) => format!("found its session store at {}", path.display()),
            Self::McpConfig(path) => format!("found its MCP config at {}", path.display()),
            Self::ConfigDir(path) => format!("found its directory at {}", path.display()),
        }
    }
}

impl AgentPlan {
    fn new(agent: AgentChoice, found: Option<Found>) -> Self {
        Self {
            agent,
            found,
            mcp: install_hooks::mcp_client_for_agent(agent),
            skills: skills_for(agent),
            plugin: false,
        }
    }

    /// [`Self::new`], with the plugin state of `agent` under `home`.
    fn detect(
        agent: AgentChoice,
        found: Option<Found>,
        home: &Path,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> Self {
        let harness = match agent {
            AgentChoice::ClaudeCode => Some(agent_plugins::PluginHarness::ClaudeCode),
            AgentChoice::Codex => Some(agent_plugins::PluginHarness::Codex),
            _ => None,
        };
        Self {
            plugin: harness
                .is_some_and(|harness| agent_plugins::enabled(harness, home, env).is_some()),
            ..Self::new(agent, found)
        }
    }

    fn wiring(&self) -> &'static str {
        match (self.plugin, self.mcp.is_some()) {
            (true, _) => "plugin",
            (false, true) => "mcp+hooks+skills",
            (false, false) => "hooks+skills",
        }
    }
}

/// The `--agent` spelling of an agent, as `--help` lists it.
fn agent_name(agent: AgentChoice) -> String {
    use clap::ValueEnum as _;
    agent.to_possible_value().map_or_else(
        || agent.kind().as_str().to_owned(),
        |value| value.get_name().to_owned(),
    )
}

/// The skill directory family an agent reads; agents without one of their
/// own read the cross-client `~/.agents/skills`.
fn skills_for(agent: AgentChoice) -> InstallSkillsAgent {
    match agent {
        AgentChoice::ClaudeCode => InstallSkillsAgent::ClaudeCode,
        AgentChoice::Grok => InstallSkillsAgent::Grok,
        AgentChoice::CopilotCli => InstallSkillsAgent::CopilotCli,
        AgentChoice::Devin => InstallSkillsAgent::Devin,
        AgentChoice::Hermes => InstallSkillsAgent::Hermes,
        AgentChoice::AntigravityCli => InstallSkillsAgent::Antigravity,
        _ => InstallSkillsAgent::Agents,
    }
}

/// The OpenCode major version to wire, resolved once so both installers use
/// the same dialect. The `opencode2` alias names V2 outright; the generic
/// `opencode` (named, or detected by the session store both majors share)
/// probes `opencode --version` the way `install-mcp` and `install-hooks` do
/// by hand, and a failed probe is an error rather than a guess.
fn opencode_dialect(
    agent: AgentChoice,
    env: &super::run::EffectiveChildEnv,
) -> Result<Option<OpenCodeDialect>> {
    match agent {
        AgentChoice::OpenCode => {
            super::opencode_dialect::resolve_generic(None, env).map(|found| Some(found.dialect))
        }
        AgentChoice::OpenCode2 => Ok(Some(OpenCodeDialect::V2)),
        _ => Ok(None),
    }
}

/// Run `setup`.
///
/// # Errors
/// When the plan cannot be built, the operator declines it (or there is no
/// terminal to ask on and no `--yes`), or any step fails; the other steps
/// still run and the error counts the failures.
pub async fn run(config: &Config, args: SetupArgs, config_path: Option<&Path>) -> Result<()> {
    let captured = config.runtime_env.process_env().to_vec();
    let env = move |name: &str| -> Option<OsString> {
        captured
            .iter()
            .find(|(key, _)| key.as_os_str() == name)
            .map(|(_, value)| value.clone())
    };
    let home = super::run::native_home(config).context("locating your home directory")?;
    let exe = std::env::current_exe().context("locating the running ai-memory binary")?;
    let exe = service_exe(&exe, &env);

    let written_config = init_config_path(config, config_path);
    let agents: Vec<(AgentChoice, Option<Found>)> = if args.agents.is_empty() {
        let mut found = detect_agents(
            &home,
            &|harness| doctor::relocated_session_dir_with(harness, &env),
            &|client| install_mcp::mcp_config_path_with(client, &env).ok(),
        );
        if let Some(dir) = muse_detected(&home, &env) {
            found.push((AgentChoice::Muse, Found::ConfigDir(dir)));
        }
        if let Some(dir) = codebuddy_detected(&home) {
            found.push((AgentChoice::CodeBuddy, Found::ConfigDir(dir)));
        }
        if let Some(dir) = ibm_bob_detected(&home) {
            found.push((AgentChoice::IbmBob, Found::ConfigDir(dir)));
        }
        if let Some(dir) = qwen_code_detected(&home) {
            found.push((AgentChoice::QwenCode, Found::ConfigDir(dir)));
        }
        found
            .into_iter()
            .map(|(agent, found)| (agent, Some(found)))
            .collect()
    } else {
        dedup(args.agents.iter().copied())
            .into_iter()
            .map(|agent| (agent, None))
            .collect()
    };
    let service = if args.no_service {
        ServicePlan::Skip("skipped (--no-service)".to_owned())
    } else {
        let spec = user_service::ServiceSpec {
            exe,
            data_dir: config.data_dir.clone(),
            config_path: written_config.clone(),
            home: home.clone(),
            xdg_config_home: env("XDG_CONFIG_HOME").map(PathBuf::from),
        };
        let probe =
            probe_setup_server(&ServerEndpoint::from_config_resolving_auth(config).await).await;
        plan_service(
            ServiceManager::current(),
            &spec,
            &config.server_url,
            &probe,
            &user_service::SystemRunner,
        )
    };
    let mut plan = SetupPlan {
        data_dir: config.data_dir.clone(),
        fresh: !written_config.exists(),
        config_path: written_config,
        service,
        agents: agents
            .into_iter()
            .map(|(agent, found)| AgentPlan::detect(agent, found, &home, &env))
            .collect(),
        detected: args.agents.is_empty(),
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&plan_document(&plan))?);
        return Ok(());
    }
    if args.web {
        use clap::ValueEnum as _;
        let accepted = AgentChoice::value_variants()
            .iter()
            .map(|agent| agent_name(*agent))
            .collect();
        let flags = if args.no_service {
            vec!["--no-service".into()]
        } else {
            Vec::new()
        };
        return super::setup_web::serve(
            serde_json::to_value(plan_document(&plan))?,
            accepted,
            flags,
            super::service_host::install_args(config, config_path),
            !args.no_open,
            args.stop_on_stdin_eof,
        )
        .await;
    }
    print_plan(&plan);
    if args.plan {
        return Ok(());
    }
    if !args.yes {
        plan.agents = choose_agents(confirm(plan.agents.len())?, plan.agents);
    }
    execute(
        config,
        config_path,
        plan,
        &user_service::SystemRunner,
        SERVICE_READY_TIMEOUT,
    )
    .await
}

fn init_config_path(config: &Config, explicit: Option<&Path>) -> PathBuf {
    explicit.map_or_else(|| config.data_dir.join("config.toml"), Path::to_path_buf)
}

fn dedup<T: PartialEq>(agents: impl Iterator<Item = T>) -> Vec<T> {
    let mut seen = Vec::new();
    for agent in agents {
        if !seen.contains(&agent) {
            seen.push(agent);
        }
    }
    seen
}

/// The agents to wire when none was named: every harness `doctor` scans whose
/// session store exists, or whose MCP config file does, under `home`, each
/// with the evidence it was found by (the store when both exist).
fn detect_agents(
    home: &Path,
    session_dir_for: &dyn Fn(ManagedHarness) -> Option<PathBuf>,
    mcp_config_for: &dyn Fn(McpClient) -> Option<PathBuf>,
) -> Vec<(AgentChoice, Found)> {
    let mut found: Vec<(AgentChoice, Found)> = Vec::new();
    for harness in doctor::SCANNED_HARNESSES
        .iter()
        .copied()
        .filter(|harness| !EXPLICIT_ONLY.contains(harness))
    {
        let Some(agent) = run_autowire::agent_choice_for_harness(harness) else {
            continue;
        };
        if found.iter().any(|(seen, _)| *seen == agent) {
            continue;
        }
        let store = native_store_root(harness, home, session_dir_for(harness).as_deref());
        let evidence = if store.is_dir() {
            Some(Found::SessionStore(store))
        } else {
            install_hooks::mcp_client_for_agent(agent)
                .and_then(mcp_config_for)
                .filter(|path| path.is_file())
                .map(Found::McpConfig)
        };
        if let Some(evidence) = evidence {
            found.push((agent, evidence));
        }
    }
    found
}

/// Every agent `setup --agent` accepts that is not in the plan, so an agent
/// detection cannot find (no session store or MCP config to look for) is
/// still one name away.
fn other_agents(planned: &[AgentPlan]) -> Vec<String> {
    use clap::ValueEnum as _;
    AgentChoice::value_variants()
        .iter()
        .filter(|agent| !planned.iter().any(|plan| plan.agent == **agent))
        .map(|agent| agent_name(*agent))
        .collect()
}

/// Whether Meta Muse Code has run for this user: its config directory
/// (`${XDG_CONFIG_HOME:-~/.config}/muse`) or its data directory
/// (`${XDG_DATA_HOME:-~/.local/share}/muse`, where its sessions live) exists.
/// Muse has no native-session adapter, so it is found by these directories
/// rather than by [`detect_agents`]. Each XDG variable counts only when it is
/// an absolute path, lexically.
fn muse_detected(home: &Path, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let xdg = |name: &str, fallback: &[&str]| {
        super::path_util::agent_config_home(env(name))
            .filter(|dir| dir.is_absolute())
            .unwrap_or_else(|| {
                fallback
                    .iter()
                    .fold(home.to_path_buf(), |dir, part| dir.join(part))
            })
            .join("muse")
    };
    [
        xdg("XDG_CONFIG_HOME", &[".config"]),
        xdg("XDG_DATA_HOME", &[".local", "share"]),
    ]
    .into_iter()
    .find(|dir| dir.is_dir())
}

/// CodeBuddy Code's directory, `~/.codebuddy`, when it exists. CodeBuddy has
/// no native-session adapter, so setup finds it by that directory rather than
/// by [`detect_agents`].
fn codebuddy_detected(home: &Path) -> Option<PathBuf> {
    Some(home.join(".codebuddy")).filter(|dir| dir.is_dir())
}

/// IBM Bob's shared configuration directory, `~/.bob`, when it exists.
fn ibm_bob_detected(home: &Path) -> Option<PathBuf> {
    Some(home.join(".bob")).filter(|dir| dir.is_dir())
}

/// Qwen Code's directory, `~/.qwen`, when it exists. Qwen has no
/// native-session adapter, so setup finds it by that directory rather than
/// by [`detect_agents`].
fn qwen_code_detected(home: &Path) -> Option<PathBuf> {
    Some(home.join(".qwen")).filter(|dir| dir.is_dir())
}

/// The binary path the service unit runs. A package manager's versioned
/// location (a Homebrew `Cellar/<formula>/<version>` or a `/nix/store` path)
/// disappears on the next upgrade or `brew cleanup` / garbage collection, so a
/// unit naming it stops starting. When the running binary lives in one, the
/// unit names a stable link that resolves to the same file instead: the path
/// as invoked, an `ai-memory` on `PATH` (`/usr/local/bin`, `~/.nix-profile/bin`,
/// `/etc/profiles/per-user/<user>/bin`), or Homebrew's `opt/<formula>` and
/// `bin` links. Without one, the resolved path is kept as before.
fn service_exe(invoked: &Path, env: &dyn Fn(&str) -> Option<OsString>) -> PathBuf {
    let Ok(resolved) = std::fs::canonicalize(invoked) else {
        return invoked.to_path_buf();
    };
    if !is_versioned_package_path(&resolved) {
        return resolved;
    }
    let name = resolved.file_name().unwrap_or(invoked.as_os_str());
    let on_path = env("PATH")
        .map(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    std::iter::once(invoked.to_path_buf())
        .chain(homebrew_links(&resolved))
        .chain(on_path)
        .find(|candidate| {
            candidate.is_absolute()
                && !is_versioned_package_path(candidate)
                && std::fs::canonicalize(candidate).is_ok_and(|target| target == resolved)
        })
        .unwrap_or(resolved)
}

/// Whether `path` sits in a store a package manager replaces per version.
fn is_versioned_package_path(path: &Path) -> bool {
    let parts: Vec<_> = path.components().map(|part| part.as_os_str()).collect();
    parts.windows(2).any(|pair| pair == ["nix", "store"]) || cellar_index(&parts).is_some()
}

/// The index of a `Cellar` component followed by `<formula>/<version>/...`.
fn cellar_index(parts: &[&std::ffi::OsStr]) -> Option<usize> {
    parts
        .iter()
        .position(|part| *part == "Cellar")
        .filter(|index| parts.len() > index + 3)
}

/// Homebrew's version-independent links to a `<prefix>/Cellar/<formula>/
/// <version>/<rest>` file: `<prefix>/opt/<formula>/<rest>` (what Homebrew's
/// own services use) and `<prefix>/<rest>`.
fn homebrew_links(resolved: &Path) -> Vec<PathBuf> {
    let parts: Vec<_> = resolved.components().map(|part| part.as_os_str()).collect();
    let Some(index) = cellar_index(&parts) else {
        return Vec::new();
    };
    let prefix: PathBuf = parts[..index].iter().collect();
    let rest: PathBuf = parts[index + 3..].iter().collect();
    vec![
        prefix.join("opt").join(parts[index + 1]).join(&rest),
        prefix.join(rest),
    ]
}

/// Decide the service step. An ai-memory server answering with no unit of ours
/// in place is left in charge (Docker, a packaged unit, a foreground
/// `serve`). An unrecognized listener also prevents a bind, but needs an
/// actionable diagnostic rather than being treated as our server.
fn plan_service(
    manager: Option<ServiceManager>,
    spec: &user_service::ServiceSpec,
    server_url: &str,
    probe: &SetupProbe,
    runner: &dyn CommandRunner,
) -> ServicePlan {
    let Some(manager) = manager else {
        return ServicePlan::Skip(
            "not supported on this platform; start the server with `ai-memory serve \
             --transport http`"
                .to_owned(),
        );
    };
    if !crate::http_client::is_loopback_url(server_url) {
        return ServicePlan::Skip(format!(
            "the configured server {server_url} is not on this machine; no local service"
        ));
    }
    if let SetupProbe::Unexpected(reason) = probe {
        return ServicePlan::Skip(format!(
            "{server_url} did not return ai-memory's /healthz JSON ({reason}); leaving the \
             listener and any existing service unchanged. Check which process owns this address, \
             then free it or set `bind = \"127.0.0.1:<free-port>\"` in {} and re-run \
             `ai-memory setup` (update an explicit server_url too). OpenCode's background \
             service also uses port 49374 by default; move it with \
             `opencode service set port <free-port>` if it is the occupant.",
            spec.config_path.display()
        ));
    }
    match user_service::render(manager, spec) {
        Ok(unit) if matches!(probe, SetupProbe::Healthy) && !unit.exists(runner) => {
            ServicePlan::Skip(format!(
                "an ai-memory server already answers at {server_url}; leaving it in charge (pass --no-service \
             to skip this check)"
            ))
        }
        Ok(unit) => ServicePlan::Install(unit),
        Err(error) => ServicePlan::Skip(format!("cannot render the service: {error:#}")),
    }
}

fn print_plan(plan: &SetupPlan) {
    println!("ai-memory setup will:");
    if plan.fresh {
        println!(
            "  - create the data dir {} and write {} (with a new root bearer token in \
             auth-tokens/default when the data dir is new)",
            plan.data_dir.display(),
            plan.config_path.display()
        );
    } else {
        println!(
            "  - keep the existing install at {} ({})",
            plan.data_dir.display(),
            plan.config_path.display()
        );
    }
    match &plan.service {
        ServicePlan::Install(unit) => println!(
            "  - install and start a {} at {}{}",
            unit.manager.describe(),
            unit.path.display(),
            if unit.manager == ServiceManager::WindowsRun {
                " (it starts `ai-memory service-host` at logon, which runs the server with no \
                 console window and restarts it after a failure)"
            } else {
                ""
            }
        ),
        ServicePlan::Skip(reason) => println!("  - service: {reason}"),
    }
    if plan.agents.is_empty() {
        println!(
            "  - wire no agent: none {}; name one with --agent <name>",
            if plan.detected {
                "was detected"
            } else {
                "was named"
            }
        );
    } else {
        println!("  - wire these agents:");
    }
    for (number, agent) in plan.agents.iter().enumerate() {
        let name = agent.agent.kind().as_str();
        let number = number + 1;
        if agent.plugin {
            println!(
                "      {number}. {name}: leave it to its enabled ai-memory plugin ({}), which \
                 carries the hooks, MCP entry and skills",
                agent_plugins::PLUGIN_ID
            );
            continue;
        }
        let mcp = if agent.mcp.is_some() {
            "MCP + hooks"
        } else {
            "hooks"
        };
        let why = agent
            .found
            .as_ref()
            .map_or_else(|| "named with --agent".to_owned(), Found::describe);
        println!("      {number}. {name}: {mcp} + skills ({why})");
    }
    println!("  - check /healthz, memory_status and doctor (read-only)");
    let others = other_agents(&plan.agents);
    if !others.is_empty() {
        println!(
            "Also supported, not in this plan (add with --agent <name>): {}",
            others.join(", ")
        );
    }
}

/// What the operator answered at the prompt.
#[derive(Debug, PartialEq, Eq)]
enum Answer {
    /// Apply the whole plan.
    All,
    /// Apply it with only these agents, as 1-based plan numbers in order.
    Only(Vec<usize>),
}

/// Ask on a terminal; refuse without one. With more than one planned agent
/// the operator may answer with the numbers of the agents to wire.
fn confirm(agents: usize) -> Result<Answer> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "nothing was changed: there is no terminal to confirm this plan on. Re-run \
             `ai-memory setup --yes` to apply it."
        );
    }
    if agents > 1 {
        print!("Proceed? [y/N], or type the numbers of the agents to wire (e.g. 1 3): ");
    } else {
        print!("Proceed? [y/N] ");
    }
    std::io::stdout().flush().ok();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .context("reading the answer")?;
    parse_answer(&answer, agents)
}

/// Read a prompt answer: yes, or (with more than one agent) a list of agent
/// numbers separated by spaces or commas. Anything else declines, and so does
/// a number outside the plan, so a typo never wires an agent nobody chose.
fn parse_answer(answer: &str, agents: usize) -> Result<Answer> {
    let answer = answer.trim();
    if matches!(answer, "y" | "Y" | "yes" | "YES" | "Yes") {
        return Ok(Answer::All);
    }
    let numbers: Vec<&str> = answer
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|part| !part.is_empty())
        .collect();
    if agents < 2 || numbers.is_empty() {
        bail!("nothing was changed");
    }
    let mut chosen = Vec::new();
    for part in numbers {
        match part.parse::<usize>() {
            Ok(number) if (1..=agents).contains(&number) => chosen.push(number),
            _ => bail!("nothing was changed: {part} is not an agent number from 1 to {agents}"),
        }
    }
    chosen.sort_unstable();
    Ok(Answer::Only(dedup(chosen.into_iter())))
}

/// The planned agents the answer kept, in plan order.
fn choose_agents(answer: Answer, agents: Vec<AgentPlan>) -> Vec<AgentPlan> {
    match answer {
        Answer::All => agents,
        Answer::Only(numbers) => agents
            .into_iter()
            .enumerate()
            .filter(|(index, _)| numbers.contains(&(index + 1)))
            .map(|(_, agent)| agent)
            .collect(),
    }
}

/// The plan as `setup --plan --json` prints it.
#[derive(Debug, serde::Serialize)]
struct PlanDocument {
    format: u32,
    data_dir: PathBuf,
    config_path: PathBuf,
    fresh: bool,
    service: ServiceDocument,
    agents: Vec<AgentDocument>,
    /// `--agent` names accepted but not in the plan.
    other_agents: Vec<String>,
    checks: [&'static str; 3],
    first_question: &'static str,
}

#[derive(Debug, serde::Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum ServiceDocument {
    Install {
        manager: &'static str,
        path: PathBuf,
    },
    Skip {
        reason: String,
    },
}

#[derive(Debug, serde::Serialize)]
struct AgentDocument {
    agent: String,
    /// `null` when the agent was named with `--agent`.
    found: Option<Found>,
    wiring: &'static str,
}

fn plan_document(plan: &SetupPlan) -> PlanDocument {
    PlanDocument {
        format: PLAN_FORMAT,
        data_dir: plan.data_dir.clone(),
        config_path: plan.config_path.clone(),
        fresh: plan.fresh,
        service: match &plan.service {
            ServicePlan::Install(unit) => ServiceDocument::Install {
                manager: unit.manager.describe(),
                path: unit.path.clone(),
            },
            ServicePlan::Skip(reason) => ServiceDocument::Skip {
                reason: reason.clone(),
            },
        },
        agents: plan
            .agents
            .iter()
            .map(|agent| AgentDocument {
                agent: agent_name(agent.agent),
                found: agent.found.clone(),
                wiring: agent.wiring(),
            })
            .collect(),
        other_agents: other_agents(&plan.agents),
        checks: ["healthz", "memory_status", "doctor"],
        first_question: FIRST_QUESTION,
    }
}

/// The closing line once every step passed: restart the wired agents, then
/// ask one of them something only ai-memory can answer.
fn next_step(agents: &[AgentPlan]) -> String {
    let names: Vec<&str> = agents
        .iter()
        .map(|agent| agent.agent.kind().as_str())
        .collect();
    next_step_for_names(&names)
}

/// [`next_step`] for agents given by name.
pub(crate) fn next_step_for_names(names: &[impl AsRef<str>]) -> String {
    let names: Vec<&str> = names.iter().map(AsRef::as_ref).collect();
    match names.as_slice() {
        [] => "ai-memory is set up, but no agent is wired; run `ai-memory setup --agent <name>`."
            .to_owned(),
        [one] => format!(
            "ai-memory is set up. Restart {one} so it loads the new hooks and MCP entry, then \
             ask it: \"{FIRST_QUESTION}\" It answers through ai-memory's tools; a new install \
             remembers nothing yet, and every session from now on is captured."
        ),
        many => format!(
            "ai-memory is set up. Restart {} so they load the new hooks and MCP entry, then ask \
             one of them: \"{FIRST_QUESTION}\" It answers through ai-memory's tools; a new \
             install remembers nothing yet, and every session from now on is captured.",
            many.join(", ")
        ),
    }
}

/// Apply the plan. Every step runs even when an earlier one failed, so the
/// report names every problem at once.
async fn execute(
    config: &Config,
    config_path: Option<&Path>,
    plan: SetupPlan,
    runner: &dyn CommandRunner,
    ready_timeout: Duration,
) -> Result<()> {
    let mut problems: Vec<String> = Vec::new();

    println!("\n== init");
    let outcome = init::initialize(config, &InitArgs { force: false }, config_path)?;
    let mut config = config.clone();
    if let Some(token) = outcome.root_token
        && config.auth.bearer_token.is_none()
    {
        config.auth.adopt_token_file(token.path, token.token);
    }
    println!(
        "config: {} {}",
        if outcome.config_written {
            "wrote"
        } else {
            "kept"
        },
        outcome.config_path.display()
    );

    println!("\n== service");
    let mut wait_for_server = false;
    match &plan.service {
        ServicePlan::Skip(reason) => println!("service: {reason}"),
        ServicePlan::Install(unit) => match user_service::install(unit, runner) {
            Ok(change) => {
                wait_for_server = change != ServiceChange::Unchanged;
                println!(
                    "service: {} {}",
                    match change {
                        ServiceChange::Installed => "installed and started",
                        ServiceChange::Updated => "updated and restarted",
                        ServiceChange::Started => "started",
                        ServiceChange::Unchanged => "already running from",
                    },
                    unit.path.display()
                );
            }
            Err(error) => problems.push(format!(
                "service: {error:#}. Start the server yourself with `ai-memory serve --transport \
                 http`, or re-run with --no-service when it runs some other way."
            )),
        },
    }
    let endpoint = ServerEndpoint::from_config_resolving_auth(&config).await;
    if wait_for_server && !wait_until_reachable(&endpoint, ready_timeout).await {
        problems.push(format!(
            "service: the server did not answer at {} within {}s",
            endpoint.url,
            ready_timeout.as_secs()
        ));
    }

    let mut skills_done = BTreeSet::new();
    for agent in &plan.agents {
        println!("\n== {}", agent.agent.kind().as_str());
        for problem in wire_agent(&config, agent, &mut skills_done) {
            problems.push(format!("{}: {problem}", agent.agent.kind().as_str()));
        }
    }

    println!("\n== self-check");
    for problem in self_check(&config, &endpoint).await {
        problems.push(format!("self-check: {problem}"));
    }

    println!();
    if config.auth.bearer_token.is_none() {
        println!("auth: none (loopback); run `ai-memory auth enable` to require a bearer token");
    } else {
        println!("auth: bearer token (owner-only file; never printed)");
    }
    if let Some(hint) = web_sign_in_hint(&config, &endpoint) {
        println!("{hint}");
    }
    if problems.is_empty() {
        let codex = codex_hook_trust(&config, &plan).await;
        println!(
            "{}",
            ready_message(codex.as_ref(), &next_step(&plan.agents))
        );
        return Ok(());
    }
    for problem in &problems {
        eprintln!("problem: {problem}");
    }
    bail!(
        "setup finished with {} problem(s); fix them and re-run `ai-memory setup`",
        problems.len()
    )
}

/// Codex's trust state for the ai-memory hooks setup left in place: the
/// hooks file `install-hooks` wrote, or the enabled plugin's hooks. `None`
/// when Codex was not wired or its state is unknown (Codex not installed or
/// not answering).
async fn codex_hook_trust(
    config: &Config,
    plan: &SetupPlan,
) -> Option<super::codex_hook_trust::CodexHookTrust> {
    let codex = plan
        .agents
        .iter()
        .find(|agent| agent.agent == AgentChoice::Codex)?;
    let cwd = std::env::current_dir().ok()?;
    if codex.plugin {
        super::codex_hook_trust::probe(
            Path::new("codex"),
            super::codex_hook_trust::HookSource::Plugin(agent_plugins::PLUGIN_ID),
            &cwd,
        )
        .await
    } else {
        doctor::codex_hook_trust(&config.runtime_env, &cwd).await
    }
}

/// The closing line of a run with no problems. Codex skips a hook the user
/// has not trusted in its `/hooks` review, so with any skipped the install
/// is not ready for Codex yet. Setup still succeeds: only the user can grant
/// that trust, inside Codex, and re-running setup would not change it.
fn ready_message(codex: Option<&super::codex_hook_trust::CodexHookTrust>, closing: &str) -> String {
    // The closing line names the agents to restart and the question to ask;
    // with Codex hooks still unapproved it follows the action required
    // instead of claiming the install is ready.
    let restart = closing
        .strip_prefix("ai-memory is set up. ")
        .unwrap_or(closing);
    match codex.filter(|trust| !trust.skipped.is_empty()) {
        None => closing.to_owned(),
        Some(trust) => {
            let skipped: Vec<String> = trust
                .skipped
                .iter()
                .map(|(event, status)| format!("{event} ({status})"))
                .collect();
            format!(
                "ai-memory is installed, but Codex has not approved its {} ai-memory hook(s) \
                 yet: {}. Codex skips a hook until you trust it, so Codex sessions are not \
                 captured until then. Action required: open Codex, run /hooks, and trust \
                 them. {restart}",
                trust.skipped.len(),
                skipped.join(", "),
            )
        }
    }
}

/// How a browser signs in to the web UI on a token-file install. With no
/// human accounts the server answers `/web` with a Basic challenge, and the
/// token file's contents are the password. The token itself is never printed.
fn web_sign_in_hint(config: &Config, endpoint: &ServerEndpoint) -> Option<String> {
    let file = config.auth.token_file.as_ref().filter(|file| file.loaded)?;
    Some(format!(
        "web UI: {} (the service runs with --enable-web); when the browser asks \
         for a username and password, use any username and the contents of {} \
         as the password",
        endpoint.build_url("/web"),
        file.path.display()
    ))
}

async fn wait_until_reachable(endpoint: &ServerEndpoint, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(probe_server(endpoint).await, ServerProbe::Reachable) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// `install-mcp`, `install-hooks` and `install-skills` for one agent, MCP
/// first so an OpenCode major-version move has one entry before the hook
/// installer reads it (the order `ai-memory run` auto-wire uses). Returns the
/// failures.
fn wire_agent(
    config: &Config,
    plan: &AgentPlan,
    skills_done: &mut BTreeSet<String>,
) -> Vec<String> {
    let mut problems = Vec::new();
    if plan.plugin {
        // Writing the settings-file wiring as well would load every skill
        // twice and register a second MCP server; the hook copies would be
        // de-duplicated, but nothing would be gained.
        println!(
            "plugin: {} is enabled; leaving the settings-file hooks, MCP entry and skills \
             alone (run `ai-memory install-hooks --agent {} --apply` to add them anyway)",
            agent_plugins::PLUGIN_ID,
            plan.agent.kind().as_str()
        );
        return problems;
    }
    let child_env = super::run::EffectiveChildEnv::from_runtime(&config.runtime_env);
    match opencode_dialect(plan.agent, &child_env) {
        Ok(dialect) => wire_mcp_and_hooks(config, plan, dialect, &mut problems),
        // A guessed major writes a plugin and MCP shape the installed OpenCode
        // ignores without an error, so nothing is written instead.
        Err(error) => problems.push(format!("skipped install-mcp and install-hooks: {error:#}")),
    }
    let skills_key = format!("{:?}", plan.skills);
    if skills_done.insert(skills_key) {
        let args = InstallSkillsArgs {
            scope: InstallSkillsScope::Global,
            agent: plan.skills,
            target_dir: None,
            print: false,
            force: false,
        };
        if let Err(error) = install_skills::run(config, args) {
            problems.push(format!("install-skills: {error:#}"));
        }
    }
    problems
}

/// `install-mcp` then `install-hooks` for one agent, in one OpenCode dialect.
fn wire_mcp_and_hooks(
    config: &Config,
    plan: &AgentPlan,
    dialect: Option<OpenCodeDialect>,
    problems: &mut Vec<String>,
) {
    if let Some(client) = plan.mcp {
        if install_mcp::existing_entry_is_session_aware(client, None, ENTRY_NAME) {
            println!("mcp: keeping the existing session-aware entry");
        } else {
            let args = InstallMcpArgs {
                client,
                server_url: None,
                name: ENTRY_NAME.to_owned(),
                auth_token: None,
                apply: true,
                config_file: None,
                session_aware: false,
                flavor: None,
                token_env_var: false,
                stdio_bridge: false,
                bridge_data_dir: None,
            };
            if let Err(error) = install_mcp::run_with_opencode_dialect(config, args, dialect) {
                problems.push(format!("install-mcp: {error:#}"));
            }
        }
    }
    let hooks = InstallHooksArgs {
        agent: plan.agent,
        hooks_dir: None,
        server_url: None,
        auth_token: None,
        as_user: None,
        apply: true,
        config_file: None,
        scope: HookInstallScope::Global,
        project_strategy: None,
        capture_assistant: false,
        capture_mode: None,
        no_capture_prompts: false,
        capture_prompts: false,
        profile: None,
    };
    if let Err(error) =
        install_hooks::run_with_opencode_dialect_and_mcp_path(config, hooks, dialect, None)
    {
        problems.push(format!("install-hooks: {error:#}"));
    }
}

/// `/healthz`, `memory_status` over MCP, and `doctor`; none of them writes a
/// file. Returns the failures.
async fn self_check(config: &Config, endpoint: &ServerEndpoint) -> Vec<String> {
    let mut problems = Vec::new();
    let healthz = endpoint.build_url("/healthz");
    match probe_setup_server(endpoint).await {
        SetupProbe::Healthy => println!("healthz: ok ({healthz})"),
        SetupProbe::Unreachable(error) => {
            problems.push(format!("{healthz} did not answer: {error}"));
            // Nothing else can pass without a server.
            return problems;
        }
        SetupProbe::Unexpected(reason) => {
            problems.push(format!(
                "{healthz} did not return ai-memory's /healthz JSON ({reason}); check the \
                 configured server URL and the process listening there"
            ));
            return problems;
        }
    }
    match memory_status(config).await {
        Ok(()) => println!("memory_status: ok"),
        Err(error) => problems.push(format!("memory_status: {error:#}")),
    }
    let doctor = DoctorArgs {
        workspace: None,
        project: None,
        since_days: 30,
        json: false,
    };
    if let Err(error) = doctor::run(config, doctor).await {
        problems.push(format!("doctor: {error:#}"));
    }
    problems
}

/// Setup needs the health response before it can leave an existing server
/// in charge or report success. The shared launch probe remains presence-only
/// for compatibility with servers that predate `/healthz`.
async fn probe_setup_server(endpoint: &ServerEndpoint) -> SetupProbe {
    let response = reqwest::Client::new()
        .get(endpoint.build_url("/healthz"))
        .timeout(Duration::from_secs(5))
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => return SetupProbe::Unreachable(error.to_string()),
    };
    match validate_healthz(response).await {
        Ok(()) => SetupProbe::Healthy,
        Err(reason) => SetupProbe::Unexpected(reason),
    }
}

async fn validate_healthz(mut response: reqwest::Response) -> Result<(), String> {
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }
    if !response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err("expected Content-Type application/json".to_owned());
    }
    // A foreign listener can return a whole page (or a stream). Bound its
    // body and never echo it into the terminal.
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| error.to_string())? {
        if body.len() + chunk.len() > 1024 {
            return Err("health response exceeds 1024 bytes".to_owned());
        }
        body.extend_from_slice(&chunk);
    }
    if serde_json::from_slice::<serde_json::Value>(&body).is_ok_and(|value| value["status"] == "ok")
    {
        Ok(())
    } else {
        Err("expected JSON with status \"ok\"".to_owned())
    }
}

/// One `memory_status` call over the server's MCP endpoint, authenticated the
/// way `mcp-bridge` authenticates, so a refused credential shows here rather
/// than in the agent's first session.
async fn memory_status(config: &Config) -> Result<()> {
    use rmcp::ServiceExt as _;
    use rmcp::model::CallToolRequestParams;

    let url = install_mcp::mcp_server_url_from_base(&config.server_url);
    let transport =
        super::mcp_bridge::upstream_transport(&url, None, config.auth.bearer_token.as_deref())?;
    let client = ().serve(transport).await.with_context(|| format!("connecting to {url}"))?;
    let result = client
        .peer()
        .call_tool(CallToolRequestParams::new("memory_status"))
        .await;
    client.cancel().await.ok();
    let result = result.context("calling memory_status")?;
    if result.is_error == Some(true) {
        bail!("memory_status reported an error");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Muse is detected by its config or data directory under the home or
    /// the absolute XDG roots, and not otherwise.
    #[test]
    fn muse_is_detected_by_its_config_or_data_directory() {
        let none = |_: &str| -> Option<OsString> { None };
        let home = tempfile::tempdir().unwrap();
        assert_eq!(muse_detected(home.path(), &none), None);
        std::fs::create_dir_all(home.path().join(".local/share/muse/sessions")).unwrap();
        assert_eq!(
            muse_detected(home.path(), &none),
            Some(home.path().join(".local/share/muse"))
        );

        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/muse")).unwrap();
        assert_eq!(
            muse_detected(home.path(), &none),
            Some(home.path().join(".config/muse"))
        );

        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(xdg.path().join("muse")).unwrap();
        let xdg_path = xdg.path().as_os_str().to_owned();
        let env = move |name: &str| (name == "XDG_CONFIG_HOME").then(|| xdg_path.clone());
        assert_eq!(
            muse_detected(home.path(), &env),
            Some(xdg.path().join("muse"))
        );
        assert_eq!(skills_for(AgentChoice::Muse), InstallSkillsAgent::Agents);
        assert_eq!(
            AgentPlan::new(AgentChoice::Muse, None).mcp,
            Some(McpClient::Muse)
        );
    }

    #[test]
    fn codebuddy_is_detected_only_when_its_home_directory_exists() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(codebuddy_detected(home.path()), None);
        std::fs::create_dir_all(home.path().join(".codebuddy")).unwrap();
        assert_eq!(
            codebuddy_detected(home.path()),
            Some(home.path().join(".codebuddy"))
        );
        assert_eq!(
            skills_for(AgentChoice::CodeBuddy),
            InstallSkillsAgent::Agents
        );
        assert_eq!(AgentPlan::new(AgentChoice::CodeBuddy, None).mcp, None);
    }

    #[test]
    fn qwen_code_is_detected_and_wires_hooks_mcp_and_shared_skills() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(qwen_code_detected(home.path()), None);
        std::fs::create_dir_all(home.path().join(".qwen")).unwrap();
        assert_eq!(
            qwen_code_detected(home.path()),
            Some(home.path().join(".qwen"))
        );
        assert_eq!(
            skills_for(AgentChoice::QwenCode),
            InstallSkillsAgent::Agents
        );
        assert_eq!(
            AgentPlan::new(AgentChoice::QwenCode, None).mcp,
            Some(McpClient::QwenCode)
        );
    }

    #[test]
    fn ibm_bob_is_detected_and_wires_hooks_mcp_and_shared_skills() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(ibm_bob_detected(home.path()), None);
        std::fs::create_dir_all(home.path().join(".bob/settings")).unwrap();
        assert_eq!(
            ibm_bob_detected(home.path()),
            Some(home.path().join(".bob"))
        );
        assert_eq!(skills_for(AgentChoice::IbmBob), InstallSkillsAgent::Agents);
        assert_eq!(
            AgentPlan::new(AgentChoice::IbmBob, None).mcp,
            Some(McpClient::IbmBob)
        );
    }

    /// A versioned Homebrew binary is recorded through the `opt/<formula>`
    /// link that survives `brew upgrade` and `brew cleanup`, falling back to
    /// the `<prefix>/bin` link; a link to some other binary never wins.
    #[cfg(unix)]
    #[test]
    fn a_cellar_binary_is_recorded_through_its_stable_homebrew_link() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let prefix = std::fs::canonicalize(root.path()).unwrap();
        let cellar_bin = prefix.join("Cellar/ai-memory/3.0.0/bin");
        std::fs::create_dir_all(&cellar_bin).unwrap();
        let real = cellar_bin.join("ai-memory");
        std::fs::write(&real, "bin").unwrap();
        let no_env = |_: &str| None;

        // No link at all: the resolved path is all there is.
        assert_eq!(service_exe(&real, &no_env), real);

        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        symlink(&real, prefix.join("bin/ai-memory")).unwrap();
        assert_eq!(service_exe(&real, &no_env), prefix.join("bin/ai-memory"));

        std::fs::create_dir_all(prefix.join("opt")).unwrap();
        symlink("../Cellar/ai-memory/3.0.0", prefix.join("opt/ai-memory")).unwrap();
        assert_eq!(
            service_exe(&real, &no_env),
            prefix.join("opt/ai-memory/bin/ai-memory")
        );

        // Control: a stable link that points at another version is refused.
        let other = tempfile::tempdir().unwrap();
        let other = std::fs::canonicalize(other.path()).unwrap();
        let old_bin = other.join("Cellar/ai-memory/2.9.0/bin");
        std::fs::create_dir_all(&old_bin).unwrap();
        let old = old_bin.join("ai-memory");
        std::fs::write(&old, "old").unwrap();
        std::fs::create_dir_all(other.join("bin")).unwrap();
        symlink(&real, other.join("bin/ai-memory")).unwrap();
        assert_eq!(service_exe(&old, &no_env), old);
    }

    /// A `/nix/store` binary is recorded through the profile link on `PATH`
    /// that resolves to it, and the path as invoked wins when it is stable.
    #[cfg(unix)]
    #[test]
    fn a_nix_store_binary_is_recorded_through_its_profile_link() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let store_bin = root.join("nix/store/abc123-ai-memory-3.0.0/bin");
        std::fs::create_dir_all(&store_bin).unwrap();
        let real = store_bin.join("ai-memory");
        std::fs::write(&real, "bin").unwrap();
        let profile_bin = root.join("home/.nix-profile/bin");
        std::fs::create_dir_all(&profile_bin).unwrap();
        symlink(&real, profile_bin.join("ai-memory")).unwrap();
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let path = std::env::join_paths([&elsewhere, &store_bin, &profile_bin]).unwrap();
        let env = move |name: &str| (name == "PATH").then(|| path.clone());

        assert_eq!(service_exe(&real, &env), profile_bin.join("ai-memory"));
        assert_eq!(service_exe(&real, &|_: &str| None), real);

        let invoked = root.join("invoked");
        symlink(&real, &invoked).unwrap();
        assert_eq!(service_exe(&invoked, &env), invoked);
    }

    /// A binary outside any versioned store keeps the resolved path, as
    /// before: a `~/.local/bin` install is already stable.
    #[cfg(unix)]
    #[test]
    fn a_plain_binary_keeps_its_resolved_path() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let real = root.join(".local/bin/ai-memory");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "bin").unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(service_exe(&link, &|_: &str| None), real);
        assert!(!is_versioned_package_path(Path::new(
            "/opt/homebrew/bin/ai-memory"
        )));
        assert!(!is_versioned_package_path(Path::new("/srv/Cellar/x")));
        assert!(is_versioned_package_path(Path::new(
            "/opt/homebrew/Cellar/ai-memory/3.0.0/bin/ai-memory"
        )));
        assert!(is_versioned_package_path(Path::new(
            "/nix/store/abc-ai-memory/bin/ai-memory"
        )));
    }

    /// An enabled plugin takes an agent's wiring over; a disabled or absent
    /// one, and an agent the plugins do not cover, keep the full plan.
    #[test]
    fn an_enabled_plugin_takes_over_the_agents_wiring() {
        use crate::commands::agent_plugins::test_support::{
            install_claude_plugin, install_codex_plugin,
        };
        let home = tempfile::tempdir().unwrap();
        let env = |_: &str| None;
        assert!(!AgentPlan::detect(AgentChoice::ClaudeCode, None, home.path(), &env).plugin);

        install_claude_plugin(home.path(), "3.0.0", false);
        assert!(!AgentPlan::detect(AgentChoice::ClaudeCode, None, home.path(), &env).plugin);
        install_claude_plugin(home.path(), "3.0.0", true);
        assert!(AgentPlan::detect(AgentChoice::ClaudeCode, None, home.path(), &env).plugin);
        assert!(!AgentPlan::detect(AgentChoice::Codex, None, home.path(), &env).plugin);
        assert!(!AgentPlan::detect(AgentChoice::Grok, None, home.path(), &env).plugin);

        install_codex_plugin(home.path());
        assert!(AgentPlan::detect(AgentChoice::Codex, None, home.path(), &env).plugin);
    }

    /// Codex skips an untrusted hook, so a run that left any unapproved says
    /// so and names `/hooks` instead of claiming the install is ready; all
    /// trusted, or an unknown state, keeps the plain ready line.
    #[test]
    fn untrusted_codex_hooks_are_not_reported_as_ready() {
        use super::super::codex_hook_trust::CodexHookTrust;
        let ready = "ai-memory is set up.";

        let untrusted = CodexHookTrust {
            trusted: vec!["stop".to_owned()],
            skipped: vec![
                ("sessionStart".to_owned(), "untrusted".to_owned()),
                ("userPromptSubmit".to_owned(), "modified".to_owned()),
            ],
        };
        let closing = next_step(&[AgentPlan::new(AgentChoice::Codex, None)]);
        let message = ready_message(Some(&untrusted), &closing);
        assert!(!message.contains(ready), "{message}");
        assert!(message.contains("run /hooks"), "{message}");
        assert!(
            message.contains("2 ai-memory hook(s)")
                && message.contains("sessionStart (untrusted)")
                && message.contains("userPromptSubmit (modified)"),
            "{message}"
        );

        let trusted = CodexHookTrust {
            trusted: vec!["sessionStart".to_owned()],
            skipped: Vec::new(),
        };
        assert!(message.contains(FIRST_QUESTION), "{message}");
        assert_eq!(ready_message(Some(&trusted), &closing), closing);
        assert_eq!(ready_message(None, &closing), closing);
        assert!(closing.starts_with(ready));
    }

    /// A token-file install learns how to open the web UI: the URL and the
    /// file to read, never the token. An inline or absent token prints no hint.
    #[test]
    fn web_hint_names_the_token_file_and_never_the_token() {
        let endpoint = ServerEndpoint::from_pair(Some("http://127.0.0.1:49374".to_owned()), None);
        let mut config = Config::default();
        assert_eq!(web_sign_in_hint(&config, &endpoint), None);

        config.auth.bearer_token = Some("inline-root-token".to_owned());
        assert_eq!(web_sign_in_hint(&config, &endpoint), None);

        let file = PathBuf::from("/data/auth-tokens/default");
        config
            .auth
            .adopt_token_file(file.clone(), "file-root-token".to_owned());
        let hint = web_sign_in_hint(&config, &endpoint).expect("token-file install gets a hint");
        assert!(hint.contains("http://127.0.0.1:49374/web"), "{hint}");
        assert!(hint.contains(&file.display().to_string()), "{hint}");
        assert!(!hint.contains("file-root-token"), "{hint}");
    }

    fn agents_of(found: &[(AgentChoice, Found)]) -> Vec<AgentChoice> {
        found.iter().map(|(agent, _)| *agent).collect()
    }

    fn plan_with(agents: Vec<AgentPlan>) -> SetupPlan {
        SetupPlan {
            data_dir: PathBuf::from("/data"),
            config_path: PathBuf::from("/data/config.toml"),
            fresh: true,
            service: ServicePlan::Skip("skipped (--no-service)".to_owned()),
            agents,
            detected: true,
        }
    }

    /// Yes applies everything; numbers keep only those agents, in plan
    /// order and once each. A number outside the plan, a stray word, a blank
    /// line, and numbers offered for a single agent all decline, so a typo
    /// never wires an agent nobody chose.
    #[test]
    fn the_prompt_answer_picks_all_some_or_none_of_the_agents() {
        assert_eq!(parse_answer("y\n", 3).unwrap(), Answer::All);
        assert_eq!(parse_answer("Yes", 1).unwrap(), Answer::All);
        assert_eq!(
            parse_answer("3, 1 3\n", 3).unwrap(),
            Answer::Only(vec![1, 3])
        );
        assert_eq!(parse_answer("2", 2).unwrap(), Answer::Only(vec![2]));
        for declined in ["", "\n", "n", "no", "4", "0", "1 x", "-1"] {
            let error = parse_answer(declined, 3).unwrap_err().to_string();
            assert!(
                error.starts_with("nothing was changed"),
                "{declined:?}: {error}"
            );
        }
        assert!(
            parse_answer("1", 1).is_err(),
            "numbers need two agents to pick from"
        );
    }

    #[test]
    fn chosen_numbers_keep_those_agents_in_plan_order() {
        let agents = vec![
            AgentPlan::new(AgentChoice::ClaudeCode, None),
            AgentPlan::new(AgentChoice::Codex, None),
            AgentPlan::new(AgentChoice::Grok, None),
        ];
        let kept = choose_agents(Answer::Only(vec![1, 3]), agents.clone());
        assert_eq!(
            kept.iter().map(|plan| plan.agent).collect::<Vec<_>>(),
            vec![AgentChoice::ClaudeCode, AgentChoice::Grok]
        );
        assert_eq!(choose_agents(Answer::All, agents.clone()), agents);
    }

    /// The JSON plan names each agent by its `--agent` spelling with the
    /// evidence it was found by, lists every other accepted agent, and
    /// carries the layout version a front end checks.
    #[test]
    fn the_json_plan_carries_agents_evidence_and_the_rest() {
        let store = PathBuf::from("/home/u/.claude/projects");
        let mut codex = AgentPlan::new(AgentChoice::Codex, None);
        codex.plugin = true;
        let plan = plan_with(vec![
            AgentPlan::new(
                AgentChoice::ClaudeCode,
                Some(Found::SessionStore(store.clone())),
            ),
            codex,
            AgentPlan::new(AgentChoice::OpenCode2, None),
        ]);
        let json = serde_json::to_value(plan_document(&plan)).unwrap();
        assert_eq!(json["format"], PLAN_FORMAT);
        assert_eq!(json["service"]["action"], "skip");
        assert_eq!(json["agents"][0]["agent"], "claude-code");
        assert_eq!(json["agents"][0]["found"]["kind"], "session_store");
        assert_eq!(
            json["agents"][0]["found"]["path"],
            store.display().to_string()
        );
        assert_eq!(json["agents"][0]["wiring"], "mcp+hooks+skills");
        assert_eq!(json["agents"][1]["wiring"], "plugin");
        assert_eq!(json["agents"][2]["agent"], "opencode2");
        assert!(json["agents"][2]["found"].is_null());
        assert_eq!(json["first_question"], FIRST_QUESTION);
        let others: Vec<&str> = json["other_agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap())
            .collect();
        assert!(
            others.contains(&"cursor") && others.contains(&"gemini-cli"),
            "{others:?}"
        );
        assert!(!others.contains(&"claude-code") && !others.contains(&"opencode2"));
        {
            use clap::ValueEnum as _;
            assert_eq!(others.len() + 3, AgentChoice::value_variants().len());
        }
    }

    /// Every accepted `--agent` value round-trips through the name the plan
    /// prints, so a name copied from the plan always parses.
    #[test]
    fn every_listed_agent_name_parses_back() {
        use clap::ValueEnum as _;
        for agent in AgentChoice::value_variants() {
            assert_eq!(
                AgentChoice::from_str(&agent_name(*agent), false).ok(),
                Some(*agent)
            );
        }
    }

    #[test]
    fn the_closing_line_names_the_agents_and_the_question() {
        let one = next_step(&[AgentPlan::new(AgentChoice::ClaudeCode, None)]);
        assert!(one.contains("Restart claude-code so it loads"), "{one}");
        assert!(one.contains(FIRST_QUESTION), "{one}");
        let two = next_step(&[
            AgentPlan::new(AgentChoice::ClaudeCode, None),
            AgentPlan::new(AgentChoice::Codex, None),
        ]);
        assert!(
            two.contains("Restart claude-code, codex so they load"),
            "{two}"
        );
        assert!(two.contains("ask one of them"), "{two}");
        let none = next_step(&[]);
        assert!(none.contains("no agent is wired"), "{none}");
        assert!(!none.contains(FIRST_QUESTION), "{none}");
    }

    fn spec(home: &Path) -> user_service::ServiceSpec {
        user_service::ServiceSpec {
            exe: PathBuf::from("/opt/ai-memory/ai-memory"),
            data_dir: home.join("data"),
            config_path: home.join("data").join("config.toml"),
            home: home.to_path_buf(),
            xdg_config_home: None,
        }
    }

    #[test]
    fn detection_finds_harness_stores_and_mcp_configs_under_home() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        // Claude Code by its session store, Codex by its MCP config only.
        std::fs::create_dir_all(home.join(".claude").join("projects")).unwrap();
        let codex_config = home.join("codex-config.toml");
        std::fs::write(&codex_config, "").unwrap();
        let mcp = |client: McpClient| (client == McpClient::Codex).then(|| codex_config.clone());
        let found = detect_agents(home, &|_| None, &mcp);
        assert_eq!(
            found,
            vec![
                (
                    AgentChoice::ClaudeCode,
                    Found::SessionStore(home.join(".claude").join("projects"))
                ),
                (AgentChoice::Codex, Found::McpConfig(codex_config.clone())),
            ]
        );
    }

    /// Control: an empty home detects nothing, and a named-but-missing MCP
    /// config path does not count as present.
    #[test]
    fn an_empty_home_detects_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.json");
        let found = detect_agents(tmp.path(), &|_| None, &|_| Some(missing.clone()));
        assert!(found.is_empty(), "{found:?}");
    }

    /// OpenCode 2 shares OpenCode's store; detection must not wire both
    /// dialects into the one config file.
    #[test]
    fn a_shared_store_detects_only_the_stable_harness() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        std::fs::create_dir_all(home.join(".local/share/opencode")).unwrap();
        std::fs::create_dir_all(home.join(".kiro/sessions/cli")).unwrap();
        let found = detect_agents(home, &|_| None, &|_| None);
        assert_eq!(
            agents_of(&found),
            vec![AgentChoice::OpenCode, AgentChoice::KiroCli]
        );
    }

    /// A relocated store (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, …) is looked for
    /// where the relocation points, not under the home.
    #[test]
    fn detection_follows_a_relocated_store() {
        let tmp = tempfile::tempdir().unwrap();
        let elsewhere = tmp.path().join("relocated-claude");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let relocate = |harness: ManagedHarness| {
            (harness == ManagedHarness::Claude).then(|| elsewhere.clone())
        };
        let found = detect_agents(&tmp.path().join("home"), &relocate, &|_| None);
        assert_eq!(
            found,
            vec![(
                AgentChoice::ClaudeCode,
                Found::SessionStore(elsewhere.clone())
            )]
        );
    }

    #[test]
    fn skills_go_to_each_agents_own_directory_or_the_shared_one() {
        assert_eq!(
            skills_for(AgentChoice::ClaudeCode),
            InstallSkillsAgent::ClaudeCode
        );
        assert_eq!(skills_for(AgentChoice::Grok), InstallSkillsAgent::Grok);
        assert_eq!(
            skills_for(AgentChoice::CopilotCli),
            InstallSkillsAgent::CopilotCli
        );
        assert_eq!(skills_for(AgentChoice::Codex), InstallSkillsAgent::Agents);
        // Antigravity has no global `~/.agents/skills`; it reads its own roots.
        assert_eq!(
            skills_for(AgentChoice::AntigravityCli),
            InstallSkillsAgent::Antigravity
        );
        assert_eq!(
            skills_for(AgentChoice::KimiCode),
            InstallSkillsAgent::Agents
        );
    }

    /// Generic `opencode` takes the probed major; the `opencode2` alias and
    /// other agents never run a probe, so an empty `PATH` cannot fail them.
    #[cfg(unix)]
    #[test]
    fn opencode_dialect_probes_only_the_generic_spelling() {
        use std::os::unix::fs::PermissionsExt as _;

        let empty = super::super::run::EffectiveChildEnv::for_tests([("PATH", "")]);
        assert_eq!(
            opencode_dialect(AgentChoice::OpenCode2, &empty).unwrap(),
            Some(OpenCodeDialect::V2)
        );
        assert_eq!(opencode_dialect(AgentChoice::Codex, &empty).unwrap(), None);
        let error = format!(
            "{:#}",
            opencode_dialect(AgentChoice::OpenCode, &empty).unwrap_err()
        );
        assert!(error.contains("was not found"), "{error}");

        for (version, expected) in [
            ("2.0.31", OpenCodeDialect::V2),
            ("1.18.34", OpenCodeDialect::V1),
        ] {
            let bin = tempfile::tempdir().unwrap();
            let opencode = bin.path().join("opencode");
            std::fs::write(&opencode, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
            std::fs::set_permissions(&opencode, std::fs::Permissions::from_mode(0o755)).unwrap();
            let env = super::super::run::EffectiveChildEnv::for_tests([("PATH", bin.path())]);
            assert_eq!(
                opencode_dialect(AgentChoice::OpenCode, &env).unwrap(),
                Some(expected),
                "{version}"
            );
        }
    }

    #[test]
    fn named_agents_are_deduplicated_in_order() {
        assert_eq!(
            dedup(
                [
                    AgentChoice::Codex,
                    AgentChoice::ClaudeCode,
                    AgentChoice::Codex
                ]
                .into_iter()
            ),
            vec![AgentChoice::Codex, AgentChoice::ClaudeCode]
        );
    }

    #[test]
    fn a_platform_without_a_manager_has_no_service_step_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let ServicePlan::Skip(reason) = plan_service(
            None,
            &spec(tmp.path()),
            "http://127.0.0.1:49374",
            &SetupProbe::Unreachable(String::new()),
            &user_service::SystemRunner,
        ) else {
            panic!("expected a skip");
        };
        assert!(reason.contains("not supported"), "{reason}");
    }

    /// The Windows logon entry is planned like a unit: installed when nothing
    /// answers, left alone when a server answers and no entry of ours exists
    /// (`reg query` finds none), and managed once the entry is there.
    #[test]
    fn the_windows_logon_entry_is_planned_like_a_unit() {
        struct Reg(Option<&'static str>);
        impl CommandRunner for Reg {
            fn run(&self, program: &str, _args: &[&str]) -> Result<user_service::CommandOutcome> {
                assert_eq!(program, "reg");
                Ok(user_service::CommandOutcome {
                    success: self.0.is_some(),
                    stdout: self.0.map_or_else(String::new, |data| {
                        format!("\r\nHKEY_CURRENT_USER\\...\\Run\r\n    ai-memory    REG_SZ    {data}\r\n")
                    }),
                    stderr: String::new(),
                })
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let url = "http://127.0.0.1:49374";
        let manager = Some(ServiceManager::WindowsRun);
        // The shared spec names a Unix binary path, which is not absolute on
        // Windows; this one is absolute on every platform.
        let spec = user_service::ServiceSpec {
            exe: tmp.path().join("ai-memory.exe"),
            ..spec(tmp.path())
        };
        assert!(matches!(
            plan_service(manager, &spec, url, &SetupProbe::Unreachable(String::new()), &Reg(None)),
            ServicePlan::Install(unit) if unit.manager == ServiceManager::WindowsRun
        ));
        assert!(matches!(
            plan_service(manager, &spec, url, &SetupProbe::Healthy, &Reg(None)),
            ServicePlan::Skip(reason) if reason.contains("already answers")
        ));
        assert!(matches!(
            plan_service(
                manager,
                &spec,
                url,
                &SetupProbe::Healthy,
                &Reg(Some("\"C:\\ai\\ai-memory.exe\" service-host"))
            ),
            ServicePlan::Install(_)
        ));
    }

    #[test]
    fn a_remote_server_gets_no_local_service() {
        let tmp = tempfile::tempdir().unwrap();
        let plan = plan_service(
            Some(ServiceManager::SystemdUser),
            &spec(tmp.path()),
            "https://memory.example.com",
            &SetupProbe::Healthy,
            &user_service::SystemRunner,
        );
        assert!(
            matches!(plan, ServicePlan::Skip(reason) if reason.contains("not on this machine"))
        );
    }

    /// A server that already answers with no unit of ours in place is not
    /// ours to replace; once our unit exists, a running server is the one it
    /// started and the unit is managed as usual.
    // Windows skips the service step before this decision.
    #[cfg(unix)]
    #[test]
    fn an_answering_server_without_our_unit_is_left_in_charge() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec(tmp.path());
        let url = "http://127.0.0.1:49374";
        let plan = plan_service(
            Some(ServiceManager::SystemdUser),
            &spec,
            url,
            &SetupProbe::Healthy,
            &user_service::SystemRunner,
        );
        assert!(matches!(plan, ServicePlan::Skip(reason) if reason.contains("already answers")));

        let ServicePlan::Install(unit) = plan_service(
            Some(ServiceManager::SystemdUser),
            &spec,
            url,
            &SetupProbe::Unreachable(String::new()),
            &user_service::SystemRunner,
        ) else {
            panic!("expected an install when nothing answers");
        };
        std::fs::create_dir_all(unit.path.parent().unwrap()).unwrap();
        std::fs::write(&unit.path, &unit.contents).unwrap();
        assert!(matches!(
            plan_service(
                Some(ServiceManager::SystemdUser),
                &spec,
                url,
                &SetupProbe::Healthy,
                &user_service::SystemRunner
            ),
            ServicePlan::Install(_)
        ));
    }

    #[tokio::test]
    async fn setup_health_probe_requires_successful_bounded_json() {
        use axum::http::StatusCode;

        for (status, content_type, body, healthy) in [
            (
                StatusCode::OK,
                "application/json",
                r#"{"status":"ok"}"#.to_owned(),
                true,
            ),
            (
                StatusCode::OK,
                "application/json; charset=utf-8",
                r#"{"status":"ok","extra":1}"#.to_owned(),
                true,
            ),
            (
                StatusCode::OK,
                "text/html",
                r#"{"status":"ok"}"#.to_owned(),
                false,
            ),
            (
                StatusCode::OK,
                "application/json",
                "<!doctype html>".to_owned(),
                false,
            ),
            (
                StatusCode::OK,
                "application/json",
                r#"{"status":"down"}"#.to_owned(),
                false,
            ),
            (
                StatusCode::OK,
                "application/json",
                r#"{"other":"ok"}"#.to_owned(),
                false,
            ),
            (StatusCode::OK, "application/json", " ".repeat(1025), false),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "application/json",
                r#"{"status":"ok"}"#.to_owned(),
                false,
            ),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let app = axum::Router::new().route(
                "/memory/healthz",
                axum::routing::get(move || async move {
                    (
                        status,
                        [(axum::http::header::CONTENT_TYPE, content_type)],
                        body,
                    )
                }),
            );
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let endpoint =
                ServerEndpoint::from_pair(Some(format!("http://{address}/memory")), None);
            let probe = probe_setup_server(&endpoint).await;
            server.abort();
            assert_eq!(
                matches!(probe, SetupProbe::Healthy),
                healthy,
                "{status} {content_type}: {probe:?}"
            );
            if !healthy {
                assert!(matches!(probe, SetupProbe::Unexpected(_)), "{probe:?}");
            }
        }
    }

    /// An unhealthy response still means the port is occupied. Do not
    /// install a second service or restart an existing one over that listener.
    #[cfg(unix)]
    #[test]
    fn an_unrecognized_listener_prevents_service_changes_even_with_an_existing_unit() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = spec(tmp.path());
        let unit = user_service::render(ServiceManager::SystemdUser, &spec).unwrap();
        for existing in [false, true] {
            if existing {
                std::fs::create_dir_all(unit.path.parent().unwrap()).unwrap();
                std::fs::write(&unit.path, &unit.contents).unwrap();
            }
            let plan = plan_service(
                Some(ServiceManager::SystemdUser),
                &spec,
                "http://127.0.0.1:49374",
                &SetupProbe::Unexpected("HTTP 503 Service Unavailable".to_owned()),
                &user_service::SystemRunner,
            );
            assert!(
                matches!(plan, ServicePlan::Skip(reason) if reason.contains("did not return ai-memory's /healthz"))
            );
            assert_eq!(unit.path.exists(), existing);
        }
    }
}
