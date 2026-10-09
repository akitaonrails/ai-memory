//! Opt-in managed cross-harness launcher.

use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use ai_memory_core::{
    AgentKind, FinishManagedRunRequest, FinishManagedRunResponse, LinkManagedRunRequest,
    ManagedRunContextResponse, ManagedRunStatus, NativeSessionIdentity, PrepareManagedRunRequest,
    PrepareManagedRunResponse, Sanitizer, SessionId,
};
use ai_memory_workstream::{
    AmbiguousNativeSession, ClaudeContinuation, ExportedTranscript, FORWARDED_ENV_NAMES,
    JailChecklistItem, JailHostFacts, JailSupport, JailToggleChoice, JailToggleKind, LaunchMode,
    LaunchPlan, LaunchRoots, ManagedHarness, NativeSessionCandidate, ai_jail_support,
    allows_native_session_adoption, apply_claude_true_yolo, apply_yolo, build_ai_jail_invocation,
    build_launch_plan, build_launch_plan_with_env_lookup, claude_continued_session,
    claude_live_background_attach_id, claude_session_ran_in_background, clean_path,
    crush_global_config_path, discover_native_session, export_transcript,
    has_native_session_selector, inside_ai_jail_here, inspect_repository,
    is_interactive_session_invocation, jail_checklist, jail_toggle, kiro_explicit_session_id,
    kiro_harness_from_source_cursor, kiro_selects_non_default_engine, kiro_selects_v2_engine,
    kiro_selects_v3_engine, kiro_v3_resume_uses_default_store, list_native_sessions,
    marked_choices, native_session_exists, native_session_in_checkout, native_store_root,
    omp_profile_flag, omp_profile_flag_env, parse_jail_toggles, store_override_vars,
    transcript_baseline, transcript_interval_digests, usable_ai_jail_here,
    wait_for_transcript_flush,
};
use anyhow::{Context as _, Result, anyhow};
use clap::ValueEnum as _;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::cli::{RunArgs, RunHarnessChoice};
use crate::commands::{path_util, resolve_scope_noting_marker_project};
use crate::config::Config;
use crate::http_client::{
    ServerEndpoint, ServerProbe, ServerResponseError, augment_connect_error, get_json, post_empty,
    post_json, post_json_no_content, probe_server,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const PREPARE_BUSY_RETRY_WINDOW: Duration = Duration::from_secs(5);
const PREPARE_BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(250);
/// Most an interactive launch waits for another launcher's lease to lapse: one
/// full server lease (90s) plus slack. A longer wait would mean the owner kept
/// renewing — a live launcher, which waiting cannot resolve.
const HELD_LEASE_MAX_WAIT: Duration = Duration::from_secs(100);
/// Margin past the reported expiry, so the retry lands after the server's clock
/// considers the lease lapsed.
const HELD_LEASE_EXPIRY_SLACK: Duration = Duration::from_secs(1);
const IMPORT_BATCH_EVENTS: usize = 400;
const IMPORT_BATCH_BYTES: usize = 1024 * 1024;
const ADOPTION_CANDIDATE_LIMIT: usize = 8;
/// Bound on `claude agents --json`, which only reads the daemon's session list.
const CLAUDE_AGENTS_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
pub(crate) struct EffectiveChildEnv {
    entries: Vec<(OsString, OsString)>,
}

impl EffectiveChildEnv {
    pub(super) fn from_runtime(env: &crate::config::RuntimeEnv) -> Self {
        Self {
            entries: env.process_env().to_vec(),
        }
    }

    pub(crate) fn with_overrides(
        env: &crate::config::RuntimeEnv,
        overrides: &[(String, String)],
    ) -> Self {
        Self::with_overrides_for_platform(env.process_env(), overrides, cfg!(windows))
    }

    pub(crate) fn with_overrides_for_platform(
        base: &[(OsString, OsString)],
        overrides: &[(String, String)],
        windows: bool,
    ) -> Self {
        let mut effective = Self {
            entries: base.to_vec(),
        };
        for (key, value) in overrides {
            effective.upsert(OsString::from(key), OsString::from(value), windows);
        }
        effective
    }

    fn upsert(&mut self, key: OsString, value: OsString, windows: bool) {
        self.entries
            .retain(|(existing, _)| !environment_keys_equal(existing, &key, windows));
        self.entries.push((key, value));
    }

    pub(crate) fn get(&self, name: &str) -> Option<&OsStr> {
        self.get_for_platform(name, cfg!(windows))
    }

    fn get_for_platform(&self, name: &str, windows: bool) -> Option<&OsStr> {
        self.entries
            .iter()
            .rev()
            .find(|(key, _)| environment_keys_equal(key, OsStr::new(name), windows))
            .map(|(_, value)| value.as_os_str())
    }

    pub(crate) fn with_additional_overrides(&self, overrides: &[(String, String)]) -> Self {
        Self::with_overrides_for_platform(&self.entries, overrides, cfg!(windows))
    }

    pub(crate) fn executable_search(&self) -> ExecutableSearch<'_> {
        ExecutableSearch {
            path: self.get("PATH"),
            extensions: self.get("PATHEXT"),
        }
    }

    pub(crate) fn apply_to_std(&self, command: &mut std::process::Command) {
        command
            .env_clear()
            .envs(self.entries.iter().map(|(key, value)| (key, value)));
    }

    fn apply_to_tokio(&self, command: &mut Command) {
        command
            .env_clear()
            .envs(self.entries.iter().map(|(key, value)| (key, value)));
    }

    #[cfg(test)]
    pub(crate) fn for_tests(
        entries: impl IntoIterator<Item = (impl Into<OsString>, impl Into<OsString>)>,
    ) -> Self {
        Self {
            entries: entries
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        }
    }

    #[cfg(test)]
    pub(crate) fn entries(&self) -> &[(OsString, OsString)] {
        &self.entries
    }
}

pub(crate) fn environment_keys_equal(left: &OsStr, right: &OsStr, windows: bool) -> bool {
    if windows {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ExecutableSearch<'a> {
    path: Option<&'a OsStr>,
    extensions: Option<&'a OsStr>,
}

const AUTO_HARNESSES: [ManagedHarness; 9] = [
    ManagedHarness::Claude,
    ManagedHarness::Codex,
    ManagedHarness::OpenCode,
    ManagedHarness::Pi,
    ManagedHarness::Crush,
    ManagedHarness::Kimi,
    ManagedHarness::CommandCode,
    ManagedHarness::Kiro,
    ManagedHarness::KiroV3,
];

#[derive(Debug, Clone)]
struct AutoSessionCandidate {
    harness: ManagedHarness,
    session: NativeSessionCandidate,
}

#[derive(Debug, Default)]
struct HeartbeatHealth {
    consecutive_failures: u64,
}

impl HeartbeatHealth {
    fn record_failure(&mut self) -> bool {
        let first = self.consecutive_failures == 0;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        first
    }

    fn record_success(&mut self) -> bool {
        let recovered = self.consecutive_failures > 0;
        self.consecutive_failures = 0;
        recovered
    }
}

fn resolve_opencode_for_launch(
    requested: Option<RunHarnessChoice>,
    executable: Option<&OsStr>,
    env: &EffectiveChildEnv,
) -> Result<Option<super::opencode_dialect::ResolvedOpenCode>> {
    let search = env.executable_search();
    let automatic = requested.is_none();
    if !(matches!(requested, Some(RunHarnessChoice::OpenCode))
        || automatic && executable_available(OsStr::new("opencode"), search))
    {
        return Ok(None);
    }
    match super::opencode_dialect::resolve_generic(executable, env) {
        Ok(resolved) => Ok(Some(resolved)),
        Err(error) if automatic => {
            eprintln!("ai-memory: automatic OpenCode scan skipped: {error:#}");
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// Run one native harness and return its exact process exit code.
pub async fn run(config: &Config, args: RunArgs) -> Result<i32> {
    let cwd = std::env::current_dir().context("getting managed run working directory")?;
    run_with_exit_prompt(config, args, &cwd).await
}

/// Run one native harness from an explicit checkout without changing the
/// parent process's working directory.
pub(super) async fn run_from(config: &Config, args: RunArgs, cwd: &Path) -> Result<i32> {
    run_from_with_wiring(
        config,
        args,
        cwd,
        &super::run_autowire::WireOverrides::default(),
    )
    .await
}

/// [`run_from`] with the autowire path injections supplied explicitly.
///
/// Production callers use [`run_from`], which passes
/// [`WireOverrides::default()`](super::run_autowire::WireOverrides) so the
/// installers resolve their real per-agent paths — behavior is byte-identical to
/// the inlined call this replaced. The overrides exist only so the `run` →
/// autowire → child-spawn seam can be exercised without writing to the
/// developer's real `$HOME`.
pub(super) async fn run_from_with_wiring(
    config: &Config,
    args: RunArgs,
    cwd: &Path,
    wire_overrides: &super::run_autowire::WireOverrides,
) -> Result<i32> {
    run_once_with_wiring(config, args, cwd, wire_overrides)
        .await
        .map(|outcome| outcome.exit_code)
}

#[derive(Debug)]
struct RunOutcome {
    exit_code: i32,
    harness: ManagedHarness,
    interactive_session: bool,
    mode: LaunchMode,
    workstream_name: String,
    /// A degraded launch made without the server: it has no workstream, so
    /// the post-run switch/rerun menu (which reuses the workstream name)
    /// must not be offered.
    degraded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterRunAction {
    Switch(RunHarnessChoice),
    Rerun,
    Quit,
}

async fn run_with_exit_prompt(config: &Config, args: RunArgs, cwd: &Path) -> Result<i32> {
    let interactive_terminal =
        io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal();
    let custom_executable = args.executable.is_some();
    let mut next_args = args;
    preserve_trailing_run_options(&mut next_args);

    loop {
        let outcome = run_once_with_wiring(
            config,
            next_args.clone(),
            cwd,
            &super::run_autowire::WireOverrides::default(),
        )
        .await?;
        if !should_offer_after_run(&outcome, interactive_terminal, custom_executable) {
            return Ok(outcome.exit_code);
        }

        let switch_env = EffectiveChildEnv::from_runtime(&config.runtime_env);
        let available = available_switch_harnesses(outcome.harness, &switch_env);
        let action = {
            let stdin = io::stdin();
            let stderr = io::stderr();
            choose_after_run(
                outcome.harness,
                &outcome.workstream_name,
                &available,
                &mut stdin.lock(),
                &mut stderr.lock(),
            )
        };
        let action = match action {
            Ok(action) => action,
            Err(error) => {
                eprintln!("ai-memory: could not read the post-run choice ({error}); exiting");
                return Ok(outcome.exit_code);
            }
        };
        if !apply_after_run_action(&mut next_args, &outcome, action) {
            return Ok(outcome.exit_code);
        }
    }
}

fn should_offer_after_run(
    outcome: &RunOutcome,
    interactive_terminal: bool,
    custom_executable: bool,
) -> bool {
    outcome.exit_code == 0
        && outcome.mode == LaunchMode::Session
        && outcome.interactive_session
        && !outcome.degraded
        && interactive_terminal
        && !custom_executable
}

fn preserve_trailing_run_options(args: &mut RunArgs) {
    args.yolo |= remove_wrapper_yolo(&mut args.native_args);
    args.true_yolo |= remove_wrapper_true_yolo(&mut args.native_args);
    args.no_autowire |= remove_wrapper_no_autowire(&mut args.native_args);
    let trailing_jail = remove_wrapper_jail(&mut args.native_args);
    if trailing_jail.jail.is_some() {
        args.jail = trailing_jail.jail;
    }
    args.no_jail |= trailing_jail.no_jail;
}

fn available_switch_harnesses(
    current: ManagedHarness,
    env: &EffectiveChildEnv,
) -> Vec<RunHarnessChoice> {
    let search = env.executable_search();
    RunHarnessChoice::value_variants()
        .iter()
        .copied()
        .filter(|choice| {
            managed_harness(*choice).agent_kind() != current.agent_kind()
                && harness_available(*choice, search)
        })
        .collect()
}

fn choose_after_run(
    current: ManagedHarness,
    workstream_name: &str,
    available: &[RunHarnessChoice],
    input: &mut impl io::BufRead,
    output: &mut impl io::Write,
) -> io::Result<AfterRunAction> {
    writeln!(
        output,
        "ai-memory: {} exited successfully in workstream '{}'.",
        current.as_str(),
        workstream_name
    )?;
    for (index, choice) in available.iter().enumerate() {
        writeln!(
            output,
            "  {}) switch to {}",
            index + 1,
            managed_harness(*choice).as_str()
        )?;
    }
    writeln!(output, "  r) run {} again", current.as_str())?;
    writeln!(output, "  q) quit (default)")?;

    loop {
        write!(output, "Select [q]: ")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(AfterRunAction::Quit);
        }
        let choice = line.trim();
        if choice.is_empty() || choice.eq_ignore_ascii_case("q") || choice == "0" {
            return Ok(AfterRunAction::Quit);
        }
        if choice.eq_ignore_ascii_case("r") {
            return Ok(AfterRunAction::Rerun);
        }
        if let Ok(index) = choice.parse::<usize>()
            && let Some(harness) = index.checked_sub(1).and_then(|i| available.get(i))
        {
            return Ok(AfterRunAction::Switch(*harness));
        }
        writeln!(output, "Choose a listed number, r, or q.")?;
    }
}

fn apply_after_run_action(
    args: &mut RunArgs,
    outcome: &RunOutcome,
    action: AfterRunAction,
) -> bool {
    let choice = match action {
        AfterRunAction::Quit => return false,
        AfterRunAction::Switch(choice) => choice,
        AfterRunAction::Rerun => run_harness_choice(outcome.harness),
    };
    args.harness = Some(choice);
    args.native_args =
        if action == AfterRunAction::Rerun && outcome.harness == ManagedHarness::KiroV3 {
            vec![OsString::from("--v3")]
        } else {
            Vec::new()
        };
    args.workstream = Some(outcome.workstream_name.clone());
    args.new_workstream = None;
    args.fresh = false;
    true
}

const fn run_harness_choice(harness: ManagedHarness) -> RunHarnessChoice {
    match harness {
        ManagedHarness::Claude => RunHarnessChoice::Claude,
        ManagedHarness::Codex => RunHarnessChoice::Codex,
        ManagedHarness::OpenCode => RunHarnessChoice::OpenCode,
        ManagedHarness::OpenCode2 => RunHarnessChoice::OpenCode2,
        ManagedHarness::Pi => RunHarnessChoice::Pi,
        ManagedHarness::Crush => RunHarnessChoice::Crush,
        ManagedHarness::Omp => RunHarnessChoice::Omp,
        ManagedHarness::Kimi => RunHarnessChoice::Kimi,
        ManagedHarness::CommandCode => RunHarnessChoice::CommandCode,
        ManagedHarness::Kiro | ManagedHarness::KiroV3 => RunHarnessChoice::Kiro,
        ManagedHarness::Grok => RunHarnessChoice::Grok,
        ManagedHarness::Antigravity => RunHarnessChoice::Antigravity,
        ManagedHarness::Copilot => RunHarnessChoice::Copilot,
    }
}

async fn run_once_with_wiring(
    config: &Config,
    args: RunArgs,
    cwd: &Path,
    wire_overrides: &super::run_autowire::WireOverrides,
) -> Result<RunOutcome> {
    let repository = inspect_repository(cwd)?;
    let home = native_home(config).context("locating native harness session storage")?;
    let automatic_harness = args.harness.is_none();
    let requested_harness = args.harness;
    let mut executable = args.executable.map(PathBuf::into_os_string);
    let mut native_args = args.native_args;
    let trailing_yolo = remove_wrapper_yolo(&mut native_args);
    let trailing_true_yolo = remove_wrapper_true_yolo(&mut native_args);
    let trailing_fresh = remove_wrapper_fresh(&mut native_args);
    let trailing_force_unlock = remove_wrapper_force_unlock(&mut native_args);
    let trailing_no_autowire = remove_wrapper_no_autowire(&mut native_args);
    let trailing_jail = remove_wrapper_jail(&mut native_args);
    let jail_request = jail_request(args.jail, args.no_jail, trailing_jail)?;
    let yolo_modes = yolo_modes(
        args.yolo || trailing_yolo,
        args.true_yolo || trailing_true_yolo,
        config.claude_true_yolo,
    );
    let yolo_requested = yolo_modes.yolo;
    let force_fresh = args.fresh || trailing_fresh;
    let force_unlock = args.force_unlock || trailing_force_unlock;
    let no_autowire = args.no_autowire || trailing_no_autowire;
    let profile = args
        .profile
        .as_deref()
        .map(|name| {
            config
                .run
                .profiles
                .get(name)
                .ok_or_else(|| unknown_run_profile(&config.run, name))
        })
        .transpose()?;
    let run_env = resolve_run_env(profile, args.env_file.as_deref(), &args.env)
        .context("resolving --profile/--env-file/--env for the managed run")?;
    let child_env = EffectiveChildEnv::with_overrides(&config.runtime_env, &run_env);
    let executable_search = child_env.executable_search();
    if automatic_harness && !native_args.is_empty() {
        return Err(anyhow!(
            "native harness arguments require an explicit harness; try `ai-memory run codex ...`"
        ));
    }
    if automatic_harness && executable.is_some() {
        return Err(anyhow!(
            "--executable requires an explicit harness; try `ai-memory run --executable <path> codex`"
        ));
    }
    let resolved_opencode =
        resolve_opencode_for_launch(requested_harness, executable.as_deref(), &child_env)?;
    if matches!(requested_harness, Some(RunHarnessChoice::OpenCode))
        && let Some(resolved) = &resolved_opencode
    {
        executable = Some(resolved.executable.clone().into_os_string());
    }
    let auto_candidates = if automatic_harness {
        filter_usable_auto_sessions(
            list_auto_sessions(
                &home,
                &repository.cwd,
                &child_env,
                resolved_opencode
                    .as_ref()
                    .map(|resolved| resolved.dialect.harness()),
            )
            .await?,
            |harness| {
                matches!(
                    harness,
                    ManagedHarness::OpenCode | ManagedHarness::OpenCode2
                ) && resolved_opencode.is_some()
                    || executable_available(OsStr::new(harness.executable()), executable_search)
            },
        )?
    } else {
        Vec::new()
    };
    let provisional_harness = match requested_harness {
        Some(RunHarnessChoice::OpenCode) => resolved_opencode
            .as_ref()
            .map(|resolved| resolved.dialect.harness())
            .context("OpenCode dialect was not resolved")?,
        Some(choice) => managed_harness_for_args(choice, &native_args),
        None => auto_candidates
            .first()
            .map(|candidate| candidate.harness)
            .ok_or_else(no_auto_session_error)?,
    };
    if automatic_harness
        && matches!(
            provisional_harness,
            ManagedHarness::OpenCode | ManagedHarness::OpenCode2
        )
        && let Some(resolved) = &resolved_opencode
    {
        executable = Some(resolved.executable.clone().into_os_string());
    }
    ensure_executable_available(
        provisional_harness,
        executable.as_deref(),
        executable_search,
    )?;
    // Resolve BOTH halves here. `--workspace` used to default to a literal
    // `default`, so a checkout whose marker declared another workspace put its
    // managed workstream in one scope while its hook-captured sessions went to
    // another — the same repository split in two.
    let (workspace, project, marker_project) = resolve_scope_noting_marker_project(
        config,
        args.workspace.as_deref(),
        args.project.as_deref(),
    )?;
    let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
    // Inside ai-jail every jail flag is moot (no nesting), so neither the
    // binary lookup nor its errors apply there.
    let jailed = inside_ai_jail_here();
    let ai_jail = if jailed { None } else { usable_ai_jail_here() };
    let jail_facts = JailHostFacts::here(&repository);
    let jail_plan = jail_decision(
        &jail_request,
        jailed,
        yolo_requested,
        interactive,
        ai_jail.is_some(),
        jail_facts.project_config,
    )?;
    // An explicit `--jail` re-runs before any managed run is prepared: the
    // jailed process opens its own, so preparing one out here would only be
    // cancelled again.
    if let (JailMode::Explicit(spec), Some(ai_jail)) = (&jail_plan.mode, &ai_jail) {
        let support = ai_jail_support(ai_jail)
            .with_context(|| format!("reading the toggles {} supports", ai_jail.display()))?;
        // With a project `.ai-jail` nothing is pre-checked, so a bare
        // `--jail` defers to that file while a listed toggle still overrides it.
        let toggles = parse_jail_toggles(spec, &jail_checklist(&jail_facts, &support), &support)?;
        if jail_plan.warn {
            let confirmation = tokio::task::spawn_blocking(|| {
                read_yolo_confirmation(false, &mut io::stdin().lock(), &mut io::stderr())
            })
            .await
            .context("waiting for the --yolo confirmation")?
            .context("reading the --yolo confirmation from stdin")?;
            if !confirmation.proceed {
                return Err(anyhow!("aborted: --yolo not confirmed"));
            }
        }
        return Err(exec_under_ai_jail(
            ai_jail,
            &support,
            &toggles,
            jail_facts.project_config,
        ));
    }
    let may_adopt_native_session = args.new_workstream.is_none() && !force_fresh;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    // Probe before any server work: a homelab down for maintenance must
    // downgrade (or, with --require-server, fail) in ~2s, not after a
    // TCP-level timeout on the prepare POST. Any HTTP answer counts as
    // reachable — including 404/405 from a server older than /healthz.
    let require_server = args.require_server || config.run.require_server;
    if let ServerProbe::Unreachable(probe_error) = probe_server(&endpoint).await {
        if require_server {
            // Fail closed exactly like the prepare below would: same
            // augmented diagnosis, same "the agent was not started" context.
            let url = endpoint.build_url("/workstream/runs");
            return Err(augment_connect_error(probe_error, &endpoint, &url)
                .context("opening managed workstream; the agent was not started"));
        }
        return run_degraded_offline(
            config,
            DegradedLaunch {
                endpoint: &endpoint,
                cwd: &repository.cwd,
                home: &home,
                harness: provisional_harness,
                executable: executable.clone(),
                native_args: &native_args,
                workspace: &workspace,
                project: &project,
                force_fresh,
                yolo_modes,
                jail_plan: &jail_plan,
                ai_jail: ai_jail.as_deref(),
                jail_facts: &jail_facts,
                no_autowire,
                child_env: &child_env,
                run_env: &run_env,
                wire_overrides,
            },
            probe_error,
        )
        .await;
    }
    let prepare = PrepareManagedRunRequest {
        workspace: workspace.clone(),
        project: project.clone(),
        cwd: repository.cwd.to_string_lossy().into_owned(),
        repo_fingerprint: repository.repo_fingerprint.clone(),
        worktree_fingerprint: repository.worktree_fingerprint.clone(),
        agent: provisional_harness.agent_kind(),
        automatic_harness,
        available_agents: unique_auto_agents(&auto_candidates),
        workstream: args.workstream,
        new_workstream: args.new_workstream,
        force_unlock,
        lease_owner: lease_owner(),
        repository_identity: remote_identity_naming(&repository.cwd, &project, marker_project),
    };
    let interrupted_before_spawn = CancellationToken::new();
    let interrupt_task = tokio::spawn(capture_interrupts(interrupted_before_spawn.clone()));
    let prepared = prepare_managed_run(&endpoint, &prepare, interactive, &interrupted_before_spawn)
        .await
        .context("opening managed workstream; the agent was not started");
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            interrupt_task.abort();
            return Err(error);
        }
    };
    if let Some(warning) = prepared.manifest_warning.as_deref() {
        eprintln!("ai-memory: warning: {warning}");
    }
    if let Err(error) = super::project_registry::record_prepared_checkout(
        config,
        &endpoint,
        &workspace,
        &project,
        &repository.cwd,
    ) {
        eprintln!(
            "ai-memory: could not refresh the client-local project link ({error:#}); continuing the managed run"
        );
    }
    let run_path = format!("/workstream/runs/{}", prepared.run_id);
    macro_rules! acquired_try {
        ($result:expr) => {
            match $result {
                Ok(value) => value,
                Err(error) => {
                    interrupt_task.abort();
                    cancel_managed_run_after_failure(&endpoint, &run_path).await;
                    return Err(error);
                }
            }
        };
    }
    if interrupted_before_spawn.is_cancelled() {
        acquired_try!(Err(anyhow!(
            "managed run interrupted before the agent started"
        )));
    }
    let privacy = acquired_try!(Sanitizer::new(&config.sanitize).map_err(anyhow::Error::from));
    if let Some(id) = prepared.native_session_id.as_deref() {
        acquired_try!(NativeSessionIdentity::parse(id, &privacy).map_err(anyhow::Error::from));
    }
    let resolved_harness = if automatic_harness {
        let resolved = prepared.resolved_agent.unwrap_or_else(|| {
            eprintln!(
                "ai-memory: the server does not support managed harness precedence; using the newest checkout-local session. Upgrade the server for established-workstream selection"
            );
            provisional_harness.agent_kind()
        });
        let selected = acquired_try!(managed_harness_from_agent(resolved).ok_or_else(|| {
            anyhow!(
                "the server selected unsupported automatic harness '{}'",
                resolved.as_str()
            )
        }));
        automatic_harness_flavor(
            selected,
            provisional_harness,
            prepared.native_session_id.as_deref(),
            resolved_opencode
                .as_ref()
                .map(|resolved| resolved.dialect.harness()),
        )
    } else {
        provisional_harness
    };
    let harness = if resolved_harness.agent_kind() == AgentKind::KiroCli {
        acquired_try!(resolve_kiro_harness(
            &native_args,
            prepared.native_session_id.as_deref(),
            prepared.source_cursor.as_deref(),
            resolved_harness,
            &home,
            &repository.cwd,
        ))
    } else {
        resolved_harness
    };
    if automatic_harness
        && matches!(
            harness,
            ManagedHarness::OpenCode | ManagedHarness::OpenCode2
        )
        && let Some(resolved) = &resolved_opencode
    {
        executable = Some(resolved.executable.clone().into_os_string());
    }
    acquired_try!(ensure_executable_available(
        harness,
        executable.as_deref(),
        executable_search,
    ));
    // Warn, and offer ai-jail, before any further native-session work — a
    // yolo re-exec under ai-jail must forward the original argv, not the
    // resolved launch plan, and restarting cleanly under ai-jail before
    // session adoption/linking begins keeps that linking simple (#16: this
    // sits around session-identity resolution, not inside it).
    acquired_try!(
        confirm_yolo_and_maybe_reexec(
            &jail_plan,
            ai_jail.as_deref(),
            &jail_facts,
            Some((&endpoint, &run_path)),
            &interrupted_before_spawn,
        )
        .await
    );
    let native_grok_rules = user_supplied_grok_rules(&native_args);
    let (mut plan, orphaned_session) = acquired_try!(build_preflighted_launch_plan(
        harness,
        executable.clone(),
        native_args.clone(),
        prepared.native_session_id.as_deref(),
        force_fresh,
        &home,
        &repository.cwd,
        &child_env,
    ));
    let interactive_session = is_interactive_session_invocation(harness, &native_args);
    let resumes_linked_session = !force_fresh
        && orphaned_session.is_none()
        && prepared.native_session_id.is_some()
        && plan.expected_session_id == prepared.native_session_id
        && !has_native_session_selector(harness, &native_args);
    if let Some(orphaned_session) = orphaned_session {
        let recorded = super::project_registry::recorded_session_store(
            config,
            &endpoint,
            harness.as_str(),
            &orphaned_session,
        )
        .unwrap_or_else(|error| {
            eprintln!(
                "ai-memory: could not read the client-local session store record ({error:#}); treating the linked session as deleted"
            );
            None
        });
        let current = effective_store(harness, &home, &repository.cwd, plan.session_dir.as_deref());
        if let Some(error) = store_mismatch_error(harness, &orphaned_session, recorded, &current) {
            acquired_try!(Err(error));
        }
        eprintln!(
            "ai-memory: linked {} session {} is missing from its native store; starting fresh and repointing workstream '{}' after the new session is established",
            harness.as_str(),
            display_session_id(&orphaned_session),
            prepared.workstream_name
        );
    } else if force_fresh && plan.mode == LaunchMode::Session {
        eprintln!(
            "ai-memory: starting a fresh {} session in workstream '{}'",
            harness.as_str(),
            prepared.workstream_name
        );
    }
    if automatic_harness
        && prepared.native_session_id.is_none()
        && prepared.may_adopt_existing_session
        && may_adopt_native_session
    {
        let candidate = acquired_try!(
            auto_candidates
                .iter()
                .find(|candidate| candidate.harness == harness)
                .context("the selected automatic harness no longer has a checkout-local session")
        );
        plan = acquired_try!(build_launch_plan_with_env_lookup(
            harness,
            executable.clone(),
            native_args.clone(),
            Some(&candidate.session.native_session_id),
            |name| child_env.get(name).map(OsStr::to_os_string),
            Some(LaunchRoots {
                home: &home,
                cwd: &repository.cwd,
            }),
        ));
        eprintln!(
            "ai-memory: continuing newest checkout-local {} session {}",
            harness.as_str(),
            display_session_id(&candidate.session.native_session_id)
        );
    } else if prepared.native_session_id.is_none()
        && prepared.may_adopt_existing_session
        && may_adopt_native_session
        && allows_native_session_adoption(harness, &native_args)
        && io::stdin().is_terminal()
        && io::stderr().is_terminal()
        && let Some(home) = native_home(config)
    {
        match list_native_sessions(
            harness,
            &home,
            &repository.cwd,
            plan.session_dir.as_deref(),
            ADOPTION_CANDIDATE_LIMIT,
        )
        .await
        {
            Ok(candidates) if !candidates.is_empty() => {
                let selection = acquired_try!(
                    choose_native_session_interactive(
                        harness,
                        prepared.workstream_name.clone(),
                        candidates,
                        &endpoint,
                        &run_path,
                        &interrupted_before_spawn,
                    )
                    .await
                );
                match selection {
                    Ok(Some(native_session_id)) => {
                        plan = acquired_try!(build_launch_plan_with_env_lookup(
                            harness,
                            executable,
                            native_args,
                            Some(&native_session_id),
                            |name| child_env.get(name).map(OsStr::to_os_string),
                            Some(LaunchRoots {
                                home: &home,
                                cwd: &repository.cwd,
                            }),
                        ));
                    }
                    Ok(None) => {}
                    Err(error) => eprintln!(
                        "ai-memory: could not read the native session choice ({error}); starting a new {} session",
                        harness.as_str()
                    ),
                }
            }
            Ok(_) => {}
            Err(error) => eprintln!(
                "ai-memory: could not inspect prior {} sessions ({error}); starting a new session",
                harness.as_str()
            ),
        }
    }
    if yolo_requested {
        // Kiro's official dangerous mode exists on the v2 engine only
        // (`--trust-all-tools`); the v3 engine replaced it with
        // permissions.yaml and documents no CLI equivalent, so the wrapper
        // maps nothing there and says so instead of failing silently.
        if harness == ManagedHarness::KiroV3
            || harness == ManagedHarness::Kiro && kiro_selects_non_default_engine(&plan.args)
        {
            eprintln!(
                "ai-memory: --yolo maps to no verified flag on the selected Kiro engine \
                 (v3 replaced --trust-all-tools with permissions.yaml); launching without it"
            );
        }
        apply_yolo(harness, &mut plan.args);
    }
    // Claude's extra "true yolo" bypass (`docs/design-yolo-safety-ai-jail.md`
    // §4), applied to the same env/args the child command is built from below.
    // Every other harness already got the plain `--yolo` mapping above, which
    // is all `--true-yolo` means for them.
    if yolo_modes.claude_true_yolo && harness == ManagedHarness::Claude {
        apply_claude_true_yolo(harness, &mut plan.args);
    }
    let planned_native_identity = acquired_try!(
        plan.expected_session_id
            .as_deref()
            .map(|id| NativeSessionIdentity::parse(id, &privacy))
            .transpose()
            .map_err(anyhow::Error::from)
    );
    let remove_kiro_home = if harness == ManagedHarness::KiroV3
        && let Some(native_session_id) = plan.expected_session_id.as_deref()
    {
        acquired_try!(kiro_v3_resume_uses_default_store(
            &home,
            &repository.cwd,
            plan.session_dir.as_deref(),
            native_session_id,
        ))
    } else {
        false
    };
    if remove_kiro_home {
        eprintln!(
            "ai-memory: Kiro v3 stored this session under the default home despite custom KIRO_HOME; using the default home for this resume"
        );
    }
    // Auto-wire this harness's ai-memory hooks + MCP the first time it launches
    // here, so managed launch "just works" for capture and recall without a
    // manual install step. One-time, idempotent, best-effort (it never blocks or
    // fails the launch); opt out with `--no-autowire` or AI_MEMORY_RUN_AUTOWIRE=false.
    // Runs once the session and its store are settled, so it wires the config
    // home the child will read, and before the child spawns so the harness
    // picks up the fresh hooks.
    // The environment the child runs with: `--env` over ai-memory's own.
    let launch_env = |name: &str| child_env.get(name).map(OsStr::to_os_string);
    if config.run_autowire && !no_autowire {
        // A Kiro v3 resume from the default store drops `KIRO_HOME` from the
        // child, so its hooks and MCP belong under the default home.
        let mut wire_env = autowire_env(harness, &run_env, &plan.args, &launch_env);
        if remove_kiro_home {
            upsert_env(
                &mut wire_env,
                "KIRO_HOME".to_string(),
                home.join(".kiro").display().to_string(),
            );
        }
        let wire_child_env = child_env.with_additional_overrides(&wire_env);
        super::run_autowire::ensure_wired_with_env(
            config,
            harness,
            wire_overrides,
            &wire_env,
            &wire_child_env,
            &repository.cwd,
            false,
        );
    }
    // A Kiro v3 resume that dropped `KIRO_HOME` runs against the default store.
    let linked_store = effective_store(
        harness,
        &home,
        &repository.cwd,
        if remove_kiro_home {
            None
        } else {
            plan.session_dir.as_deref()
        },
    );
    if plan.mode == LaunchMode::Session
        && let Some(native_identity) = &planned_native_identity
    {
        acquired_try!(
            post_json_no_content(
                &endpoint,
                &format!("{run_path}/link"),
                &LinkManagedRunRequest {
                    native_session_id: native_identity.as_str().to_owned(),
                },
            )
            .await
            .context("linking the managed native session; the agent was not started")
        );
        remember_session_store(
            config,
            &endpoint,
            harness,
            native_identity.as_str(),
            &linked_store,
        );
    }
    // Claude refuses `--resume` on a background session that is still running
    // and points at `claude attach`, so open it that way instead. The 2.1.288
    // lifecycle check observed no new SessionStart from the attach client;
    // native flags are omitted because it connects to the existing session.
    let attached_background = if harness == ManagedHarness::Claude
        && plan.mode == LaunchMode::Session
        && resumes_linked_session
        && let Some(native_session_id) = plan.expected_session_id.as_deref()
    {
        claude_background_attach_id(
            &plan,
            &home,
            &repository.cwd,
            &child_env,
            native_session_id,
            executable_search,
        )
        .await
    } else {
        None
    };
    if let Some(attach_id) = &attached_background {
        eprintln!(
            "ai-memory: Claude session {} is still running in the background; attaching to it (`claude attach {attach_id}`). Native arguments and the workstream context packet do not reach an attached session",
            plan.expected_session_id
                .as_deref()
                .map(display_session_id)
                .unwrap_or_default()
        );
        plan.args = vec![OsString::from("attach"), OsString::from(attach_id)];
    }

    let crush_context = if harness == ManagedHarness::Crush && plan.mode == LaunchMode::Session {
        let source = crush_context_source(&home, &repository.cwd, launch_env);
        acquired_try!(prepare_crush_context(&endpoint, &run_path, &source).await)
    } else {
        None
    };
    let grok_context = if harness == ManagedHarness::Grok && plan.mode == LaunchMode::Session {
        // `--rules` is single-use in Grok's argument parser, so a user-supplied
        // rules flag wins and the packet stays undelivered (it is redelivered
        // on the next managed run that can accept it).
        if native_grok_rules {
            eprintln!(
                "ai-memory: --rules was supplied natively; the workstream context packet will be delivered on a later run"
            );
            None
        } else {
            acquired_try!(fetch_grok_context(&endpoint, &run_path).await)
        }
    } else {
        None
    };
    if let Some(context) = &grok_context {
        plan.args
            .extend([OsString::from("--rules"), OsString::from(context)]);
    }
    if interrupted_before_spawn.is_cancelled() {
        acquired_try!(Err(anyhow!(
            "managed run interrupted before the agent started"
        )));
    }

    let started_at = SystemTime::now();
    // Spawn the resolved file rather than the bare name: on Windows the name
    // alone can match an unlaunchable extension-less shim (see
    // `resolve_program`). Falling back to the plan's own value keeps an
    // unresolvable program reaching the spawn error below, which explains it.
    let program = resolve_program(&plan.program, executable_search)
        .unwrap_or_else(|| plan.program.clone().into());
    let mut command = Command::new(&program);
    child_env.apply_to_tokio(&mut command);
    command.args(&plan.args).current_dir(&repository.cwd);
    command
        .env("AI_MEMORY_RUN_ID", prepared.run_id.to_string())
        .env(
            "AI_MEMORY_WORKSTREAM_ID",
            prepared.workstream_id.to_string(),
        )
        .env("AI_MEMORY_HOOK_URL", endpoint.build_url(""))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // A blank home override is unset for session import, auto-wire and the
    // Crush context config; drop it from the child as well, or the harness
    // would read a blank-named directory under the checkout that nothing else
    // follows.
    for name in blank_home_overrides(harness, &launch_env) {
        command.env_remove(name);
    }
    if remove_kiro_home {
        command.env_remove("KIRO_HOME");
    }
    if let Some(context) = &crush_context {
        command.env("CRUSH_GLOBAL_CONFIG", context.path());
    }
    let child = command.spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(spawn_error) => {
            let spawn_message = spawn_error.to_string();
            let request = FinishManagedRunRequest {
                native_session_id: plan.expected_session_id,
                source_cursor: prepared.source_cursor,
                events: Vec::new(),
                complete: true,
                checkpoint: repository.checkpoint,
                losses: vec![format!(
                    "native process could not be started: {spawn_message}"
                )],
                exit_code: None,
            };
            let finished = finish_with_retry(&endpoint, &run_path, &request)
                .await
                .is_ok();
            interrupt_task.abort();
            if !finished {
                cancel_managed_run_after_failure(&endpoint, &run_path).await;
            }
            return Err(anyhow!(spawn_message)).context(format!(
                "starting managed {} executable {}",
                harness.as_str(),
                plan.program.to_string_lossy()
            ));
        }
    };
    if (harness == ManagedHarness::Crush || grok_context.is_some())
        && let Err(error) = post_empty_with_retry(
            &endpoint,
            &format!("{run_path}/context/accept"),
            "acknowledging managed context",
        )
        .await
    {
        eprintln!(
            "ai-memory: {error}; the context may be delivered again on the next {} run",
            harness.as_str()
        );
    }

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;
    let mut heartbeat_health = HeartbeatHealth::default();
    let status = loop {
        tokio::select! {
            result = child.wait() => break acquired_try!(result.context("waiting for managed harness")),
            _ = heartbeat.tick() => {
                let _ = send_managed_heartbeat(
                    &endpoint,
                    &run_path,
                    &mut heartbeat_health,
                ).await;
            }
        }
    };
    let exit_code = status.code().unwrap_or(1);

    let server_status = if plan.mode == LaunchMode::Session {
        get_json::<ManagedRunStatus>(&endpoint, &run_path, &[])
            .await
            .ok()
    } else {
        None
    };
    let own_session_id = match own_native_session(
        &plan,
        harness,
        &home,
        &repository.cwd,
        server_status.as_ref(),
        &privacy,
    ) {
        Ok(own) => own,
        Err(error) => {
            // Only a linked session is checked, so the server reported its id.
            if harness.lacks_session_end_hook()
                && let Some(linked) = server_status
                    .as_ref()
                    .and_then(|status| status.native_session_id.as_deref())
            {
                eprintln!(
                    "ai-memory: not finalizing the {} session: could not tie it to this run ({error:#}); if it was this run's, run `ai-memory finalize-session --agent {} --reopen --session-id {} --workspace {} --project {}`",
                    harness.as_str(),
                    harness.agent_kind().as_str(),
                    SessionId::from_native(linked),
                    super::render_shared::shell_quote(&workspace),
                    super::render_shared::shell_quote(&project)
                );
            }
            None
        }
    };
    let native_session_id = acquired_try!(
        resolve_native_session_after_run(
            &plan,
            harness,
            &home,
            &repository.cwd,
            started_at,
            server_status.as_ref(),
            &privacy,
        )
        .await
    );
    let recovery_start_cursor =
        if native_session_id.as_deref() == prepared.native_session_id.as_deref() {
            prepared.source_cursor.clone()
        } else {
            None
        };
    let (transcript, exact_export_available) = if plan.mode == LaunchMode::Session
        && harness.transcript_capability() == ai_memory_workstream::TranscriptCapability::Export
    {
        let exported = export_after_flush(
            harness,
            &home,
            &repository.cwd,
            plan.session_dir.as_deref(),
            native_session_id.as_deref(),
            recovery_start_cursor.as_deref(),
        )
        .await;
        let exact = exported.losses.iter().all(|loss| {
            !loss.starts_with("native session id could not be discovered")
                && !loss.starts_with("native transcript import failed")
        }) && exported.source_cursor.is_some();
        (exported, exact)
    } else {
        (
            ExportedTranscript {
                native_session_id: native_session_id.clone().unwrap_or_default(),
                ..ExportedTranscript::default()
            },
            false,
        )
    };
    let recovery_event_digests =
        exact_export_available.then(|| transcript_interval_digests(&transcript.events));
    let recovery_cursor = exact_export_available
        .then(|| transcript.source_cursor.clone())
        .flatten();
    let checkpoint = inspect_repository(&repository.cwd)
        .map(|identity| identity.checkpoint)
        .unwrap_or(repository.checkpoint);
    let imported = match import_batches(
        &endpoint,
        &run_path,
        transcript,
        checkpoint,
        Some(exit_code),
        &privacy,
    )
    .await
    {
        Ok(imported) => imported,
        // The child has already exited; an unreachable server must not turn
        // its exit code into an error. The transcript is not recorded, so say
        // how to repair it once the server is back. Protocol-level failures
        // (a live server's 4xx, an unparsable body) keep the hard fail: they
        // need an operator, not a server restart.
        Err(error) if is_unreachable_error(&error) => {
            eprintln!(
                "{}",
                offline_import_warning(
                    harness,
                    native_session_id.as_deref(),
                    &workspace,
                    &project,
                    &error,
                )
            );
            journal_finish_failed_run(
                config,
                &endpoint,
                harness,
                native_session_id.as_deref(),
                &plan,
                &workspace,
                &project,
                &repository.cwd,
                started_at,
                exit_code,
                prepared.run_id,
                recovery_event_digests.as_deref(),
                recovery_start_cursor.as_deref(),
                recovery_cursor.as_deref(),
                &privacy,
            );
            interrupt_task.abort();
            return Ok(RunOutcome {
                exit_code,
                harness,
                interactive_session,
                mode: plan.mode,
                workstream_name: prepared.workstream_name,
                degraded: false,
            });
        }
        Err(error) => acquired_try!(Err(error)),
    };

    // Sessions whose id was unknown at launch (a fresh Codex session, say) are
    // linked by the finish request above, so record their store here.
    if plan.mode == LaunchMode::Session
        && let Some(native_session_id) = native_session_id.as_deref()
        && plan.expected_session_id.as_deref() != Some(native_session_id)
    {
        remember_session_store(config, &endpoint, harness, native_session_id, &linked_store);
    }
    if plan.mode == LaunchMode::Session
        && prepared.sync_through > prepared.sync_after
        && !server_status.is_some_and(|status| status.context_delivered)
        && attached_background.is_none()
    {
        eprintln!(
            "ai-memory: this harness did not acknowledge its managed context packet; refresh its ai-memory hooks before the next run"
        );
    }
    eprintln!(
        "ai-memory: workstream '{}' saved {imported} new event(s)",
        prepared.workstream_name
    );
    interrupt_task.abort();
    if let Some((session, finalized)) = finalize_hookless_session(
        config,
        harness,
        own_session_id.as_deref(),
        &workspace,
        &project,
    )
    .await
    {
        let agent = harness.agent_kind().as_str();
        match finalized {
            Ok(ids) if !ids.is_empty() => {
                eprintln!("ai-memory: finalized the {agent} session {session}");
            }
            // No session under that id for this agent and owner in this
            // scope: its hooks are not installed or never fired, or they
            // filed it under another workspace/project.
            Ok(_) => {}
            Err(error) => eprintln!(
                "ai-memory: could not finalize the {agent} session {session} ({error:#}); run `ai-memory finalize-session --agent {agent} --reopen --session-id {session} --workspace {} --project {}`",
                super::render_shared::shell_quote(&workspace),
                super::render_shared::shell_quote(&project)
            ),
        }
    }
    Ok(RunOutcome {
        exit_code,
        harness,
        interactive_session,
        mode: plan.mode,
        workstream_name: prepared.workstream_name,
        degraded: false,
    })
}

/// How long finalizing may hold up the harness's exit code.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything the local half of [`run_once_with_wiring`] resolved before the
/// server probe: the inputs a degraded (server-unreachable) launch still has.
struct DegradedLaunch<'a> {
    endpoint: &'a ServerEndpoint,
    cwd: &'a Path,
    home: &'a Path,
    harness: ManagedHarness,
    executable: Option<OsString>,
    native_args: &'a [OsString],
    workspace: &'a str,
    project: &'a str,
    force_fresh: bool,
    yolo_modes: YoloModes,
    jail_plan: &'a JailPlan,
    ai_jail: Option<&'a Path>,
    jail_facts: &'a JailHostFacts,
    no_autowire: bool,
    child_env: &'a EffectiveChildEnv,
    run_env: &'a [(String, String)],
    wire_overrides: &'a super::run_autowire::WireOverrides,
}

/// Launch the harness without the server: warn loudly, do no server work at
/// all, and return the child's exit code. See `docs/managed-workstreams.md`
/// ("Degraded offline launches") for the exact lost/kept contract.
async fn run_degraded_offline(
    config: &Config,
    launch: DegradedLaunch<'_>,
    probe_error: reqwest::Error,
) -> Result<RunOutcome> {
    eprintln!(
        "{}",
        offline_launch_warning(
            launch.endpoint,
            launch.harness,
            config.hook_spool.max_attempts,
            probe_error,
        )
    );
    let interrupted = CancellationToken::new();
    let interrupt_task = tokio::spawn(capture_interrupts(interrupted.clone()));
    let outcome = degraded_launch(config, &launch, &interrupted).await;
    interrupt_task.abort();
    outcome
}

async fn degraded_launch(
    config: &Config,
    launch: &DegradedLaunch<'_>,
    interrupted: &CancellationToken,
) -> Result<RunOutcome> {
    // No lease exists, so there is no mutual exclusion against another
    // launcher in this checkout. The launch therefore never adopts or resumes
    // a session implicitly — only an explicit native session selector
    // resumes, because the user named the session themselves and no
    // arbitration is needed. Everything else starts a fresh session.
    let (mut plan, _) = build_preflighted_launch_plan(
        launch.harness,
        launch.executable.clone(),
        launch.native_args.to_vec(),
        None,
        launch.force_fresh,
        launch.home,
        launch.cwd,
        launch.child_env,
    )?;
    // The `--yolo` warning and ai-jail offer still apply; a degraded launch
    // just has no prepared lease for the re-exec to release.
    confirm_yolo_and_maybe_reexec(
        launch.jail_plan,
        launch.ai_jail,
        launch.jail_facts,
        None,
        interrupted,
    )
    .await?;
    if launch.yolo_modes.yolo {
        // Same Kiro caveat as the online path: v3 has no verified dangerous
        // mode, so the wrapper maps nothing there and says so.
        if launch.harness == ManagedHarness::KiroV3
            || launch.harness == ManagedHarness::Kiro && kiro_selects_non_default_engine(&plan.args)
        {
            eprintln!(
                "ai-memory: --yolo maps to no verified flag on the selected Kiro engine \
                 (v3 replaced --trust-all-tools with permissions.yaml); launching without it"
            );
        }
        apply_yolo(launch.harness, &mut plan.args);
    }
    if launch.yolo_modes.claude_true_yolo && launch.harness == ManagedHarness::Claude {
        apply_claude_true_yolo(launch.harness, &mut plan.args);
    }
    let launch_env = |name: &str| launch.child_env.get(name).map(OsStr::to_os_string);
    // Auto-wire keeps the hook install (it is local and idempotent, and
    // capture must spool offline) but skips the MCP registration — pointing
    // the harness at a server that just proved unreachable would degrade
    // every later start for no recall. The unwritten sentinel lets the next
    // online launch complete the wiring.
    if config.run_autowire && !launch.no_autowire {
        let wire_env = autowire_env(launch.harness, launch.run_env, &plan.args, &launch_env);
        let wire_child_env = launch.child_env.with_additional_overrides(&wire_env);
        super::run_autowire::ensure_wired_with_env(
            config,
            launch.harness,
            launch.wire_overrides,
            &wire_env,
            &wire_child_env,
            launch.cwd,
            true,
        );
    }
    let executable_search = launch.child_env.executable_search();
    let program = resolve_program(&plan.program, executable_search)
        .unwrap_or_else(|| plan.program.clone().into());
    let mut command = Command::new(&program);
    launch.child_env.apply_to_tokio(&mut command);
    command.args(&plan.args).current_dir(launch.cwd);
    // No AI_MEMORY_RUN_ID / AI_MEMORY_WORKSTREAM_ID: no run exists on the
    // server, and the SessionEnd hook appends `managed_run` when the run id
    // is set — attributing the offline session to a run that never happened.
    // Removed explicitly: a launcher itself started under a managed run would
    // otherwise leak its own (now stale) ids into the offline child.
    // AI_MEMORY_HOOK_URL stays so the hooks spool against the right server.
    command
        .env_remove("AI_MEMORY_RUN_ID")
        .env_remove("AI_MEMORY_WORKSTREAM_ID")
        .env("AI_MEMORY_HOOK_URL", launch.endpoint.build_url(""))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for name in blank_home_overrides(launch.harness, &launch_env) {
        command.env_remove(name);
    }
    let prelaunch_sessions = snapshot_native_sessions(
        launch.harness,
        launch.home,
        launch.cwd,
        plan.session_dir.as_deref(),
    )
    .await;
    let prelaunch_cursor = if let Some(session) = plan.expected_session_id.as_deref()
        && launch.harness.transcript_capability()
            == ai_memory_workstream::TranscriptCapability::Export
    {
        match native_session_exists(
            launch.harness,
            launch.home,
            launch.cwd,
            plan.session_dir.as_deref(),
            session,
        ) {
            Ok(true) => Some(
                transcript_baseline(
                    launch.harness,
                    launch.home,
                    launch.cwd,
                    plan.session_dir.as_deref(),
                    session,
                )
                .await,
            ),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        }
    } else {
        None
    };
    let started_at = SystemTime::now();
    let child = command.spawn().map_err(|spawn_error| {
        let spawn_message = spawn_error.to_string();
        anyhow!(spawn_message).context(format!(
            "starting managed {} executable {}",
            launch.harness.as_str(),
            plan.program.to_string_lossy()
        ))
    })?;
    let mut child = child;
    let status = child.wait().await.context("waiting for managed harness")?;
    let exit_code = status.code().unwrap_or(1);
    let spool = super::hook_spool::spool_health(&super::hook_spool::spool_dir(&config.data_dir));
    eprintln!(
        "ai-memory: this run was not recorded on the server; {}",
        spooled_events_note(&spool)
    );
    journal_degraded_run(
        config,
        launch,
        &plan,
        exit_code,
        started_at,
        prelaunch_sessions,
        prelaunch_cursor,
    )
    .await;
    Ok(RunOutcome {
        exit_code,
        harness: launch.harness,
        interactive_session: is_interactive_session_invocation(launch.harness, launch.native_args),
        mode: plan.mode,
        workstream_name: "(offline)".to_string(),
        degraded: true,
    })
}

/// Journal a degraded session launch so `ai-memory recover` can close it out
/// once the server returns. Best-effort by contract: the child has already
/// exited and its exit code is preserved above everything else, so a journal
/// problem may only warn. Passthrough launches are not journaled — they hold
/// no session and no transcript, so there is nothing to recover.
async fn journal_degraded_run(
    config: &Config,
    launch: &DegradedLaunch<'_>,
    plan: &LaunchPlan,
    exit_code: i32,
    started_at: SystemTime,
    prelaunch_sessions: Option<std::collections::HashSet<String>>,
    prelaunch_cursor: Option<Result<String>>,
) {
    if plan.mode != LaunchMode::Session {
        return;
    }
    let native_session_id = match Sanitizer::new(&config.sanitize) {
        Ok(privacy) => resolve_degraded_session(
            plan,
            launch.harness,
            launch.home,
            launch.cwd,
            started_at,
            prelaunch_sessions.as_ref(),
            &privacy,
        )
        .await
        .unwrap_or_else(|error| {
            eprintln!(
                "ai-memory: could not uniquely identify the session this run used ({error:#}); \
                 nothing was journaled; preserve the local transcript and run `ai-memory \
                 backfill --force --session <id>` after identifying it"
            );
            None
        }),
        Err(error) => {
            eprintln!(
                "ai-memory: could not record this run for recovery (privacy config: {error}); \
                 nothing was journaled"
            );
            return;
        }
    };
    let Some(native_session_id) = native_session_id else {
        eprintln!(
            "ai-memory: no native session was discovered, so this run could not be journaled; \
             retain the harness transcript for manual backfill"
        );
        return;
    };
    let source_cursor = match prelaunch_cursor {
        Some(Ok(cursor)) => Some(cursor),
        Some(Err(error)) => {
            eprintln!(
                "ai-memory: could not record a safe delta baseline for resumed session {} ({error:#}); nothing was journaled; retain the native transcript for manual repair",
                display_session_id(&native_session_id)
            );
            return;
        }
        None => None,
    };
    let spool_only = launch.harness.transcript_capability()
        == ai_memory_workstream::TranscriptCapability::SpoolOnly;
    let (event_digests, final_cursor) = if spool_only {
        (None, None)
    } else {
        let exact_export = match export_exact_after_run(
            launch.harness,
            launch.home,
            launch.cwd,
            plan.session_dir.as_deref(),
            &native_session_id,
            source_cursor.as_deref(),
        )
        .await
        {
            Ok(export) => export,
            Err(error) => {
                eprintln!(
                    "ai-memory: could not capture an exact post-run transcript bound for session {} ({error:#}); nothing was journaled; retain the native transcript for manual repair",
                    display_session_id(&native_session_id)
                );
                return;
            }
        };
        let Some(final_cursor) = exact_export.source_cursor.clone() else {
            eprintln!(
                "ai-memory: the {} transcript adapter produced no post-run cursor; nothing was journaled; retain the native transcript for manual repair",
                launch.harness.as_str()
            );
            return;
        };
        (
            Some(transcript_interval_digests(&exact_export.events)),
            Some(final_cursor),
        )
    };
    let entry = super::recovery::JournalEntry {
        id: uuid::Uuid::new_v4().to_string(),
        recorded_at: jiff::Timestamp::now().to_string(),
        interval_started_ms: system_time_ms(started_at),
        interval_ended_ms: system_time_ms(SystemTime::now()),
        kind: super::recovery::JournalKind::DegradedRun,
        harness: launch.harness.as_str().to_string(),
        native_session_id: Some(native_session_id),
        session_dir: absolute_session_dir(plan.session_dir.as_deref(), launch.cwd)
            .map(|path| path.to_string_lossy().into_owned()),
        cwd: launch.cwd.to_string_lossy().into_owned(),
        workspace: launch.workspace.to_string(),
        project: launch.project.to_string(),
        server_url: launch.endpoint.identity(),
        run_id: None,
        event_digests,
        spool_only,
        source_cursor,
        final_cursor,
        correlated_events_seen: false,
        correlated_delivery_confirmed: false,
        correlated_loss: false,
        exit_code: Some(exit_code),
        failures: Vec::new(),
    };
    match super::recovery::append_journal_entry(&config.data_dir, entry) {
        Ok(()) => {
            eprintln!(
                "ai-memory: recorded for recovery — run `ai-memory recover` once the server \
                 is back to replay this session's exact native transcript"
            );
        }
        Err(error) => {
            eprintln!(
                "ai-memory: could not record this run for recovery ({error:#}); the native \
                 transcript remains in the harness store and can be imported with `ai-memory \
                 backfill --force --session <id>`"
            );
        }
    }
}

const DEGRADED_SESSION_SNAPSHOT_LIMIT: usize = 2_000;

async fn snapshot_native_sessions(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> Option<std::collections::HashSet<String>> {
    let sessions = list_native_sessions(
        harness,
        home,
        cwd,
        session_dir,
        DEGRADED_SESSION_SNAPSHOT_LIMIT + 1,
    )
    .await
    .ok()?;
    if sessions.len() > DEGRADED_SESSION_SNAPSHOT_LIMIT {
        return None;
    }
    Some(
        sessions
            .into_iter()
            .map(|session| session.native_session_id)
            .collect(),
    )
}

#[allow(clippy::too_many_arguments)]
async fn resolve_degraded_session(
    plan: &LaunchPlan,
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    started_at: SystemTime,
    before: Option<&std::collections::HashSet<String>>,
    sanitizer: &Sanitizer,
) -> Result<Option<String>> {
    if let Some(expected) = plan.expected_session_id.as_deref() {
        return NativeSessionIdentity::parse(expected, sanitizer)
            .map(NativeSessionIdentity::into_string)
            .map(Some)
            .map_err(anyhow::Error::from);
    }
    let Some(before) = before else {
        return Err(anyhow!(
            "pre-launch native-session snapshot was unavailable"
        ));
    };
    let after = list_native_sessions(
        harness,
        home,
        cwd,
        plan.session_dir.as_deref(),
        DEGRADED_SESSION_SNAPSHOT_LIMIT + 1,
    )
    .await?;
    if after.len() > DEGRADED_SESSION_SNAPSHOT_LIMIT {
        return Err(anyhow!(
            "post-launch native-session snapshot exceeded its {}-record limit",
            DEGRADED_SESSION_SNAPSHOT_LIMIT
        ));
    }
    let native_session_id = unique_new_session(after, before, started_at)?;
    NativeSessionIdentity::parse(&native_session_id, sanitizer)
        .map(NativeSessionIdentity::into_string)
        .map(Some)
        .map_err(anyhow::Error::from)
}

fn unique_new_session(
    after: Vec<NativeSessionCandidate>,
    before: &std::collections::HashSet<String>,
    started_at: SystemTime,
) -> Result<String> {
    let mut new = after
        .into_iter()
        .filter(|candidate| {
            candidate.updated_at + Duration::from_secs(2) >= started_at
                && !before.contains(&candidate.native_session_id)
        })
        .collect::<Vec<_>>();
    if new.len() != 1 {
        return Err(anyhow!(
            "expected one new native session after launch, found {}",
            new.len()
        ));
    }
    Ok(new.remove(0).native_session_id)
}

fn system_time_ms(time: SystemTime) -> u64 {
    time.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn absolute_session_dir(session_dir: Option<&Path>, cwd: &Path) -> Option<PathBuf> {
    session_dir.map(|path| {
        if path.is_absolute() {
            clean_path(path)
        } else {
            clean_path(&cwd.join(path))
        }
    })
}

/// The one-line account of what remains in the local hook spool after a
/// degraded run, mirroring `ai-memory status`'s offline-spool phrasing.
fn spooled_events_note(spool: &super::hook_spool::SpoolHealth) -> String {
    match spool.pending {
        0 => "no hook events remain spooled locally".to_string(),
        1 => format!(
            "1 hook event remains spooled locally (oldest {})",
            super::status::spool_age_line(spool.oldest_age_ms),
        ),
        pending => format!(
            "{pending} hook events remain spooled locally (oldest {})",
            super::status::spool_age_line(spool.oldest_age_ms),
        ),
    }
}

/// The loud one-time warning a degraded launch prints: what is lost, what
/// still works, and how the local spool is bounded. Reuses
/// [`augment_connect_error`]'s diagnosis so the operator sees the same
/// server-URL explanation a hard failure would print.
fn offline_launch_warning(
    endpoint: &ServerEndpoint,
    harness: ManagedHarness,
    spool_max_attempts: u32,
    probe_error: reqwest::Error,
) -> String {
    let diagnosis = augment_connect_error(probe_error, endpoint, &endpoint.build_url(""));
    format!(
        "ai-memory: WARNING: the ai-memory server at {} is unreachable — launching {} \
         WITHOUT server support.\n  {diagnosis:#}\n  Lost this run: no workstream lease or \
         cross-harness context, no immediate transcript import, no handoff delivery.\n  Still \
         works: lifecycle hooks spool events locally, and the native transcript is journaled \
         after session discovery. Run `ai-memory recover` when the server returns to drain the \
         spool and replay the exact transcript through the sanitized backfill path.\n  Spooled \
         events are dropped after {} failed drain passes, 7 days, or the 10,000-file spool \
         cap (set AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS=0 for a planned outage). Pass \
         --require-server to fail instead of degrading.",
        endpoint.url,
        harness.as_str(),
        spool_max_attempts,
    )
}

/// The warning printed when the server became unreachable after the child
/// exited, so its transcript could not be imported: the child's exit code is
/// preserved and the run is repairable by hand once the server returns.
fn offline_import_warning(
    harness: ManagedHarness,
    native_session_id: Option<&str>,
    workspace: &str,
    project: &str,
    error: &anyhow::Error,
) -> String {
    let session = native_session_id
        .map(SessionId::from_native)
        .map_or_else(|| "unknown".to_string(), |session| session.to_string());
    format!(
        "ai-memory: warning: {error:#}; the transcript for {} session {session} in {}/{} was \
         not imported and the orphaned lease will expire automatically within 90 seconds. The \
         harness exit code is preserved. Run `ai-memory recover` once the server is back.",
        harness.as_str(),
        workspace,
        project,
    )
}

/// Journal a run whose transcript import failed on an unreachable server, so
/// `ai-memory recover` can re-export and re-import it once the server
/// returns. The journal keeps only locators and recomputes the repository
/// checkpoint during recovery — never transcript content. Best-effort: the
/// child's exit code is already preserved, so a journal problem may only warn.
#[allow(clippy::too_many_arguments)]
fn journal_finish_failed_run(
    config: &Config,
    endpoint: &ServerEndpoint,
    harness: ManagedHarness,
    native_session_id: Option<&str>,
    plan: &LaunchPlan,
    workspace: &str,
    project: &str,
    cwd: &Path,
    started_at: SystemTime,
    exit_code: i32,
    run_id: ai_memory_core::ManagedRunId,
    event_digests: Option<&[String]>,
    source_cursor: Option<&str>,
    final_cursor: Option<&str>,
    sanitizer: &Sanitizer,
) {
    let entry = super::recovery::JournalEntry {
        id: uuid::Uuid::new_v4().to_string(),
        recorded_at: jiff::Timestamp::now().to_string(),
        interval_started_ms: system_time_ms(started_at),
        interval_ended_ms: system_time_ms(SystemTime::now()),
        kind: super::recovery::JournalKind::FinishFailed,
        harness: harness.as_str().to_string(),
        native_session_id: native_session_id
            .and_then(|id| NativeSessionIdentity::parse(id, sanitizer).ok())
            .map(NativeSessionIdentity::into_string),
        session_dir: absolute_session_dir(plan.session_dir.as_deref(), cwd)
            .map(|path| path.to_string_lossy().into_owned()),
        cwd: cwd.to_string_lossy().into_owned(),
        workspace: workspace.to_string(),
        project: project.to_string(),
        server_url: endpoint.identity(),
        run_id: Some(run_id.to_string()),
        event_digests: event_digests.map(<[String]>::to_vec),
        spool_only: harness.transcript_capability()
            == ai_memory_workstream::TranscriptCapability::SpoolOnly,
        source_cursor: source_cursor.map(str::to_owned),
        final_cursor: final_cursor.map(str::to_owned),
        correlated_events_seen: false,
        correlated_delivery_confirmed: false,
        correlated_loss: false,
        exit_code: Some(exit_code),
        failures: Vec::new(),
    };
    match super::recovery::append_journal_entry(&config.data_dir, entry) {
        Ok(()) => {
            eprintln!(
                "ai-memory: recorded for recovery — run `ai-memory recover` once the server \
                 is back to re-import this transcript"
            );
        }
        Err(error) => {
            eprintln!(
                "ai-memory: could not record this run for recovery ({error:#}); retain the \
                 native transcript and original run id for manual repair"
            );
        }
    }
}

/// Whether a managed-run request failed because the server could not be
/// reached at all (connect error or timeout) rather than answering with a
/// protocol-level failure. Only the former may be downgraded to a warning
/// after the child has exited: an answered error needs an operator, while an
/// unreachable one only needs the server back.
fn is_unreachable_error(error: &anyhow::Error) -> bool {
    let answered = error
        .chain()
        .any(|cause| cause.downcast_ref::<ServerResponseError>().is_some());
    let transport = error
        .chain()
        .any(|cause| cause.downcast_ref::<reqwest::Error>().is_some());
    !answered && transport
}

/// Finalizes the run's own session when the harness has no native session-end
async fn finalize_hookless_session(
    config: &Config,
    harness: ManagedHarness,
    own_session_id: Option<&str>,
    workspace: &str,
    project: &str,
) -> Option<(SessionId, Result<Vec<String>>)> {
    let native_session_id = own_session_id?;
    if !harness.lacks_session_end_hook() {
        return None;
    }
    let session = SessionId::from_native(native_session_id);
    let args = crate::cli::FinalizeSessionArgs {
        agent: harness.agent_kind(),
        workspace: Some(workspace.to_string()),
        project: Some(project.to_string()),
        all_owners: false,
        all: false,
        session_id: Some(session),
        // These harnesses keep capturing under the same id when a session is
        // resumed, so a later run must be able to end it again. A re-end
        // with nothing new since the last one changes nothing on the server.
        reopen: true,
        json: false,
    };
    // Tokio keeps SIGINT once it has been captured, so listen afresh: Ctrl-C
    // skips a slow finalize instead of being dropped.
    let finalized = tokio::select! {
        finalized = tokio::time::timeout(
            FINALIZE_TIMEOUT,
            super::finalize_session::finalize(config, &args),
        ) => finalized
            .unwrap_or_else(|_| Err(anyhow!("timed out after {}s", FINALIZE_TIMEOUT.as_secs()))),
        Ok(()) = tokio::signal::ctrl_c() => Err(anyhow!("interrupted")),
    };
    Some((session, finalized.map(|(_, _, ids)| ids)))
}

async fn capture_interrupts(interrupted: CancellationToken) {
    while tokio::signal::ctrl_c().await.is_ok() {
        interrupted.cancel();
    }
}

async fn send_managed_heartbeat(
    endpoint: &ServerEndpoint,
    run_path: &str,
    health: &mut HeartbeatHealth,
) -> Result<()> {
    send_managed_heartbeat_with_timeout(endpoint, run_path, health, HEARTBEAT_REQUEST_TIMEOUT).await
}

async fn send_managed_heartbeat_with_timeout(
    endpoint: &ServerEndpoint,
    run_path: &str,
    health: &mut HeartbeatHealth,
    request_timeout: Duration,
) -> Result<()> {
    let path = format!("{run_path}/heartbeat");
    let result = tokio::time::timeout(request_timeout, post_empty(endpoint, &path))
        .await
        .map_err(|_| anyhow!("request timed out after {request_timeout:?}"))
        .and_then(|result| result);
    match &result {
        Ok(()) if health.record_success() => {
            eprintln!(
                "ai-memory: server connection restored; managed workstream heartbeat resumed"
            );
        }
        Ok(()) => {}
        Err(error) if health.record_failure() => {
            tracing::debug!(error = %error, "managed workstream heartbeat became unavailable");
            eprintln!(
                "ai-memory: server unavailable; managed workstream heartbeat will retry quietly"
            );
        }
        Err(_) => {}
    }
    result
}

async fn cancel_managed_run_after_failure(endpoint: &ServerEndpoint, run_path: &str) {
    if let Err(error) = post_empty_with_retry(
        endpoint,
        &format!("{run_path}/cancel"),
        "releasing the managed workstream after a launcher failure",
    )
    .await
    {
        eprintln!(
            "ai-memory: {error}; the orphaned lease will expire automatically within 90 seconds"
        );
    }
}

/// What the invocation asked for about ai-jail
/// (docs/design-yolo-safety-ai-jail.md §5).
#[derive(Debug, Clone, PartialEq, Eq)]
enum JailRequest {
    /// Neither flag: the interactive `--yolo` offer.
    Unspecified,
    /// `--no-jail`.
    Never,
    /// `--jail` (empty list: smart defaults) or `--jail=LIST`.
    Explicit(String),
}

/// `--jail`/`--no-jail` found among the native arguments (after the harness).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TrailingJail {
    jail: Option<String>,
    no_jail: bool,
}

/// Strip `--jail`, `--jail=LIST`, and `--no-jail` from the native arguments,
/// like `--yolo`: they are wrapper flags and must never reach the harness. The
/// last `--jail` wins, matching clap's handling before the harness name.
fn remove_wrapper_jail(args: &mut Vec<OsString>) -> TrailingJail {
    let mut found = TrailingJail::default();
    args.retain(|arg| {
        let Some(arg) = arg.to_str() else {
            return true;
        };
        if arg == "--no-jail" {
            found.no_jail = true;
            return false;
        }
        if arg == "--jail" {
            found.jail = Some(String::new());
            return false;
        }
        if let Some(list) = arg.strip_prefix("--jail=") {
            found.jail = Some(list.to_owned());
            return false;
        }
        true
    });
    found
}

/// Merge the clap-parsed flags with any found after the harness name. The
/// trailing `--jail` is later in argv, so it wins over a leading one.
fn jail_request(
    jail: Option<String>,
    no_jail: bool,
    trailing: TrailingJail,
) -> Result<JailRequest> {
    let jail = trailing.jail.or(jail);
    let no_jail = no_jail || trailing.no_jail;
    match (jail, no_jail) {
        (Some(_), true) => Err(anyhow!("--jail and --no-jail cannot be used together")),
        (Some(list), false) => Ok(JailRequest::Explicit(list)),
        (None, true) => Ok(JailRequest::Never),
        (None, false) => Ok(JailRequest::Unspecified),
    }
}

/// How this launch treats ai-jail.
#[derive(Debug, Clone, PartialEq, Eq)]
enum JailMode {
    /// Run unjailed (or already jailed): no question, no re-exec.
    Stay,
    /// After the `--yolo` warning, ask "Re-run inside it?" and, on yes, show
    /// the toggle checklist — unless a project `.ai-jail` owns the toggles.
    Offer {
        /// Whether to show the checklist (no project `.ai-jail`).
        checklist: bool,
    },
    /// Re-exec now with this toggle list, without asking.
    Explicit(String),
}

/// Whether to show the `--yolo` warning, and what to do about ai-jail.
#[derive(Debug, Clone, PartialEq, Eq)]
struct JailPlan {
    warn: bool,
    mode: JailMode,
}

/// The pure decision table behind `--jail`/`--no-jail`/`--yolo`
/// (docs/design-yolo-safety-ai-jail.md §5). Inside ai-jail nothing applies (no
/// nesting, no warning). Otherwise the `--yolo` warning shows whenever `--yolo`
/// runs interactively; an explicit `--jail` re-execs even non-interactively
/// (that is how scripts get a jailed run) and fails rather than silently
/// running unjailed when ai-jail is not usable; without either flag, the offer
/// is only made interactively, after the warning, when ai-jail is usable. A
/// project `.ai-jail` (`project_config`) replaces the checklist: ai-jail loads
/// that file itself, so the offer re-execs with no toggles of ours.
fn jail_decision(
    request: &JailRequest,
    jailed: bool,
    yolo: bool,
    interactive: bool,
    usable: bool,
    project_config: bool,
) -> Result<JailPlan> {
    if jailed {
        return Ok(JailPlan {
            warn: false,
            mode: JailMode::Stay,
        });
    }
    let warn = yolo && interactive;
    let mode = match request {
        JailRequest::Never => JailMode::Stay,
        JailRequest::Explicit(_) if !usable => {
            return Err(anyhow!(
                "--jail: ai-jail is not usable on this host (it needs `ai-jail` plus its sandbox \
                 backend, bwrap on Linux or sandbox-exec on macOS, and does not run on Windows); \
                 refusing to run unjailed — drop --jail to run without it"
            ));
        }
        JailRequest::Explicit(list) => JailMode::Explicit(list.clone()),
        JailRequest::Unspecified if warn && usable => JailMode::Offer {
            checklist: !project_config,
        },
        JailRequest::Unspecified => JailMode::Stay,
    };
    Ok(JailPlan { warn, mode })
}

/// A confirmation line's yes/no verdict: `Enter`/empty, `y`, or `yes`
/// (case-insensitive) proceed; only an explicit `n`/`no` declines. Anything
/// else also proceeds — this is a `[Y/n]` prompt, not a strict allowlist.
fn yolo_decision(confirmed_line: &str) -> bool {
    !matches!(
        confirmed_line.trim().to_ascii_lowercase().as_str(),
        "n" | "no"
    )
}

/// The two yes/no answers `read_yolo_confirmation` can return: whether to
/// proceed with `--yolo` at all, and, only when ai-jail is offered, whether
/// to re-exec under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct YoloConfirmation {
    proceed: bool,
    jail: bool,
}

/// Print the `--yolo` warning (docs/design-yolo-safety-ai-jail.md §1), and
/// the ai-jail offer when available (§2), then read the confirming line(s).
/// `input`/`output` are injected so the wording and default-yes semantics
/// are unit-tested without a real terminal.
fn read_yolo_confirmation(
    ai_jail_available: bool,
    input: &mut impl io::BufRead,
    output: &mut impl io::Write,
) -> io::Result<YoloConfirmation> {
    writeln!(
        output,
        "⚠  --yolo runs every tool call with no confirmation. An agent can delete"
    )?;
    writeln!(
        output,
        "   files, run any command, and reach the network unsupervised."
    )?;
    write!(output, "   Proceed? [Y/n] ")?;
    output.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    if !yolo_decision(&line) {
        return Ok(YoloConfirmation {
            proceed: false,
            jail: false,
        });
    }
    if !ai_jail_available {
        return Ok(YoloConfirmation {
            proceed: true,
            jail: false,
        });
    }
    write!(
        output,
        "ai-jail is installed. Re-run this session inside it? [Y/n] "
    )?;
    output.flush()?;
    let mut jail_line = String::new();
    input.read_line(&mut jail_line)?;
    Ok(YoloConfirmation {
        proceed: true,
        jail: yolo_decision(&jail_line),
    })
}

/// Invalid checklist answers tolerated before giving up. Aborting is the safe
/// end: guessing could mount credentials the user meant to leave out.
const CHECKLIST_MAX_INVALID: usize = 3;

/// One checklist answer's effect on the marks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ChecklistAnswer {
    /// Empty line: accept as marked.
    Accept,
    /// Marks changed; show the list again.
    Changed,
    /// Not understood; nothing changed.
    Invalid(String),
}

/// Apply one checklist line: empty accepts, `all`/`none` set every mark, and
/// row numbers (separated by spaces or commas) flip those rows. A line with
/// any bad number changes nothing.
fn apply_checklist_answer(line: &str, checked: &mut [bool]) -> ChecklistAnswer {
    let answer = line.trim().to_ascii_lowercase();
    match answer.as_str() {
        "" => return ChecklistAnswer::Accept,
        "all" | "none" => {
            checked.fill(answer == "all");
            return ChecklistAnswer::Changed;
        }
        _ => {}
    }
    let mut rows = Vec::new();
    for token in answer
        .split([' ', ',', '\t'])
        .filter(|token| !token.is_empty())
    {
        match token.parse::<usize>() {
            Ok(row) if (1..=checked.len()).contains(&row) => rows.push(row - 1),
            _ => {
                return ChecklistAnswer::Invalid(format!(
                    "`{token}` is not a row number between 1 and {}; press Enter to accept, or type numbers, `all`, or `none`",
                    checked.len()
                ));
            }
        }
    }
    for row in rows {
        checked[row] = !checked[row];
    }
    ChecklistAnswer::Changed
}

fn render_checklist(
    items: &[JailChecklistItem],
    checked: &[bool],
    output: &mut impl io::Write,
) -> io::Result<()> {
    writeln!(
        output,
        "Enable in the jail (Enter = as marked; numbers flip, e.g. \"2 4\"; \"all\" / \"none\"):"
    )?;
    let width = items
        .iter()
        .map(|item| item.toggle.label.chars().count())
        .max()
        .unwrap_or(0);
    for (index, (item, on)) in items.iter().zip(checked).enumerate() {
        let mark = if *on { 'x' } else { ' ' };
        writeln!(
            output,
            "  [{mark}] {}) {:<width$}   {}",
            index + 1,
            item.toggle.label,
            item.toggle.note
        )?;
    }
    write!(output, "> ")?;
    output.flush()
}

/// The interactive toggle checklist (docs/design-yolo-safety-ai-jail.md §5),
/// line-based with injected `input`/`output` like [`read_yolo_confirmation`].
/// Enter or EOF accepts the marks. The result is every row as the user saw
/// it ([`marked_choices`]): checked rows `--X`, unchecked rows `--no-X`, so an
/// unchecked row stays off even if the user's own ai-jail config enables it.
fn read_jail_checklist(
    items: &[JailChecklistItem],
    input: &mut impl io::BufRead,
    output: &mut impl io::Write,
) -> io::Result<Vec<JailToggleChoice>> {
    let mut checked: Vec<bool> = items.iter().map(|item| item.checked).collect();
    let mut invalid = 0;
    loop {
        render_checklist(items, &checked, output)?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(output)?;
            break;
        }
        match apply_checklist_answer(&line, &mut checked) {
            ChecklistAnswer::Accept => break,
            ChecklistAnswer::Changed => {}
            ChecklistAnswer::Invalid(message) => {
                invalid += 1;
                writeln!(output, "{message}")?;
                if invalid >= CHECKLIST_MAX_INVALID {
                    return Err(io::Error::other(
                        "too many unrecognized answers to the ai-jail checklist",
                    ));
                }
            }
        }
    }
    let marked: Vec<JailChecklistItem> = items
        .iter()
        .zip(checked)
        .map(|(item, checked)| JailChecklistItem {
            toggle: item.toggle,
            checked,
        })
        .collect();
    Ok(marked_choices(&marked))
}

/// The one-line account of what the jailed re-run enables, grouped the way
/// the risk differs: credentials the agent can use, host capabilities, the
/// user's own `no-X` entries, and — for an explicit selection — the visible
/// rows it left out (forced off, summarized rather than spelled out) — and
/// whether the project `.ai-jail` supplies the rest.
fn jail_summary(toggles: &[JailToggleChoice], project_config: bool) -> String {
    let labels = |kind: JailToggleKind| -> Vec<&'static str> {
        toggles
            .iter()
            .filter(|choice| choice.enable)
            .filter_map(|choice| jail_toggle(choice.stem))
            .filter(|toggle| toggle.kind == kind)
            .map(|toggle| toggle.label)
            .collect()
    };
    let mut parts = Vec::new();
    for (heading, kind) in [
        ("credentials", JailToggleKind::Credential),
        ("capabilities", JailToggleKind::Capability),
    ] {
        let names = labels(kind);
        if !names.is_empty() {
            parts.push(format!("{heading}: {}", names.join(", ")));
        }
    }
    let off: Vec<String> = toggles
        .iter()
        .filter(|choice| !choice.enable && !choice.implied)
        .map(JailToggleChoice::flag)
        .collect();
    if !off.is_empty() {
        parts.push(format!("forced off: {}", off.join(" ")));
    }
    if toggles.iter().any(|choice| choice.implied) {
        parts.push("everything else in the checklist off".to_owned());
    }
    if project_config {
        parts.push("plus the project .ai-jail".to_owned());
    }
    if parts.is_empty() {
        "ai-memory: re-running inside ai-jail with no extra mounts".to_owned()
    } else {
        format!(
            "ai-memory: re-running inside ai-jail ({})",
            parts.join("; ")
        )
    }
}

/// The post-prepare `--yolo` gate (docs/design-yolo-safety-ai-jail.md). A
/// no-op unless `plan.warn`. On confirmation it either returns (unjailed, or
/// the user declined the ai-jail offer) or, on accepting the offer and the
/// checklist, cancels this process's already-prepared managed run and re-execs
/// the original invocation under `ai-jail` — which never returns on success.
/// `lease` is the prepared run to release before that re-exec; a degraded
/// offline launch passes `None` (it holds no lease).
async fn confirm_yolo_and_maybe_reexec(
    plan: &JailPlan,
    ai_jail: Option<&Path>,
    facts: &JailHostFacts,
    lease: Option<(&ServerEndpoint, &str)>,
    interrupted: &CancellationToken,
) -> Result<()> {
    if !plan.warn {
        return Ok(());
    }
    if interrupted.is_cancelled() {
        return Err(anyhow!("managed run interrupted before the agent started"));
    }
    // `jail_decision` only offers when ai-jail resolved, and `ai_jail` is the
    // exact binary the re-exec runs. An explicit `--jail` re-exec'd earlier
    // and never reaches here.
    let (ai_jail, show_checklist) = match plan.mode {
        JailMode::Offer { checklist } => (ai_jail, checklist),
        JailMode::Stay | JailMode::Explicit(_) => (None, false),
    };
    let ai_jail_available = ai_jail.is_some();
    let confirmation = tokio::task::spawn_blocking(move || {
        read_yolo_confirmation(
            ai_jail_available,
            &mut io::stdin().lock(),
            &mut io::stderr(),
        )
    })
    .await
    .context("waiting for the --yolo confirmation")?
    .context("reading the --yolo confirmation from stdin")?;
    if !confirmation.proceed {
        return Err(anyhow!("aborted: --yolo not confirmed"));
    }
    let (true, Some(ai_jail)) = (confirmation.jail, ai_jail) else {
        return Ok(());
    };
    let support = ai_jail_support(ai_jail)
        .with_context(|| format!("reading the toggles {} supports", ai_jail.display()))?;
    let items = jail_checklist(facts, &support);
    // A project `.ai-jail` owns the toggles: ai-jail loads it itself, so no
    // checklist and no flags of ours.
    let toggles = if !show_checklist || items.is_empty() {
        Vec::new()
    } else {
        tokio::task::spawn_blocking(move || {
            read_jail_checklist(&items, &mut io::stdin().lock(), &mut io::stderr())
        })
        .await
        .context("waiting for the ai-jail checklist")?
        .context("reading the ai-jail checklist from stdin")?
    };
    // The re-exec replaces this process (or, off Unix, this process exits
    // once the child does), so its own prepared lease must be released here
    // rather than left to the 90s orphan timeout — the jailed re-run opens
    // its own workstream cleanly. A degraded launch holds no lease.
    if let Some((endpoint, run_path)) = lease {
        cancel_managed_run_after_failure(endpoint, run_path).await;
    }
    Err(exec_under_ai_jail(
        ai_jail,
        &support,
        &toggles,
        facts.project_config,
    ))
}

/// Re-exec the original invocation under `ai_jail` with `toggles`, after the
/// one-line summary. Returns only on failure.
fn exec_under_ai_jail(
    ai_jail: &Path,
    support: &JailSupport,
    toggles: &[JailToggleChoice],
    project_config: bool,
) -> anyhow::Error {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            return anyhow!(error)
                .context("resolving the current executable for the ai-jail re-exec");
        }
    };
    let forwarded: Vec<OsString> = std::env::args_os().skip(1).collect();
    let present: Vec<&str> = FORWARDED_ENV_NAMES
        .iter()
        .copied()
        .filter(|name| std::env::var_os(name).is_some())
        .collect();
    // `--agent-state` is a bare toggle: enable it so the harness's own login
    // state survives ai-jail's ephemeral private home (ai-jail derives the
    // per-harness state location from the wrapped `run <harness>` it parses).
    // `--no-save-config` keeps ai-jail from writing these transient flags into
    // the project `.ai-jail` (see `build_ai_jail_invocation`).
    let jail_args = build_ai_jail_invocation(
        &exe,
        &forwarded,
        &present,
        true,
        support.supports("no-save-config"),
        toggles,
    );
    eprintln!("{}", jail_summary(toggles, project_config));
    reexec_under_ai_jail(ai_jail, &jail_args)
}

/// Replace this process with `<ai_jail> <jail_args>` on Unix (never returns
/// on success); elsewhere, spawn it, wait, and exit with its status (also
/// never returns). `ai_jail` is the path [`usable_ai_jail_here`] resolved,
/// never a bare name re-resolved through `PATH`.
#[cfg(unix)]
fn reexec_under_ai_jail(ai_jail: &Path, jail_args: &[OsString]) -> anyhow::Error {
    use std::os::unix::process::CommandExt as _;
    let error = std::process::Command::new(ai_jail).args(jail_args).exec();
    anyhow!(error).context(format!("re-executing under {}", ai_jail.display()))
}

#[cfg(not(unix))]
fn reexec_under_ai_jail(ai_jail: &Path, jail_args: &[OsString]) -> anyhow::Error {
    match std::process::Command::new(ai_jail).args(jail_args).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(error) => anyhow!(error).context(format!("spawning {}", ai_jail.display())),
    }
}

/// The session this run can prove is its own: the one it launched or resumed,
/// or the one its child linked under the run's id. A concurrent launch in the
/// same checkout cannot link under this run's id, while discovery can only
/// guess from timing. A descendant process inherits the id too, so the linked
/// session must also be this checkout's.
fn own_native_session(
    plan: &LaunchPlan,
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    server_status: Option<&ManagedRunStatus>,
    sanitizer: &Sanitizer,
) -> Result<Option<String>> {
    if plan.mode == LaunchMode::Passthrough {
        return Ok(None);
    }
    if let Some(native_session_id) = &plan.expected_session_id {
        return Ok(Some(
            NativeSessionIdentity::parse(native_session_id, sanitizer)?.into_string(),
        ));
    }
    let Some(linked) = server_status
        .filter(|status| status.native_session_linked)
        .and_then(|status| status.native_session_id.as_deref())
    else {
        return Ok(None);
    };
    let linked = NativeSessionIdentity::parse(linked, sanitizer)?;
    let linked = linked.as_str();
    let in_checkout =
        native_session_in_checkout(harness, home, cwd, plan.session_dir.as_deref(), linked)
            .with_context(|| format!("reading native session {linked}"))?;
    Ok(in_checkout.then(|| linked.to_string()))
}

/// A Claude session that attached to a background session (`/resume` on one
/// the Claude Code daemon hosts) or was `/clear`ed went on in another
/// transcript, so the workstream follows it. Otherwise the next launch resumes
/// the launch session, which holds none of the later conversation.
fn follow_claude_continued_session(
    plan: &LaunchPlan,
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    own: String,
    started_at: SystemTime,
) -> String {
    if harness != ManagedHarness::Claude {
        return own;
    }
    match claude_continued_session(home, cwd, plan.session_dir.as_deref(), &own, started_at) {
        Ok(Some(continuation)) => {
            let how = match &continuation {
                ClaudeContinuation::Background(_) => "continued in background session",
                ClaudeContinuation::Cleared(_) => "was cleared into session",
            };
            eprintln!(
                "ai-memory: Claude session {} {how} {}; the workstream now follows it",
                display_session_id(&own),
                display_session_id(continuation.session_id())
            );
            continuation.session_id().to_owned()
        }
        Ok(None) => own,
        Err(error) => {
            eprintln!(
                "ai-memory: could not check whether Claude session {} moved to another session ({error}); keeping it",
                display_session_id(&own)
            );
            own
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn resolve_native_session_after_run(
    plan: &LaunchPlan,
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    started_at: SystemTime,
    server_status: Option<&ManagedRunStatus>,
    sanitizer: &Sanitizer,
) -> Result<Option<String>> {
    if plan.mode == LaunchMode::Passthrough {
        return Ok(None);
    }
    if let Some(id) = server_status.and_then(|status| status.native_session_id.as_deref()) {
        NativeSessionIdentity::parse(id, sanitizer)?;
    }
    if let Ok(Some(own)) = own_native_session(plan, harness, home, cwd, server_status, sanitizer) {
        return Ok(Some(follow_claude_continued_session(
            plan, harness, home, cwd, own, started_at,
        )));
    }
    let linked = server_status
        .filter(|status| status.native_session_linked)
        .and_then(|status| status.native_session_id.as_deref());
    let fresh = !has_native_session_selector(harness, &plan.args);
    let discovered = match discover_native_session(
        harness,
        home,
        cwd,
        plan.session_dir.as_deref(),
        started_at,
        fresh,
    )
    .await
    {
        Ok(discovered) => discovered,
        // The session the run was prepared with is no evidence either: this
        // launch may not have touched it while another one did.
        Err(error) if error.is::<AmbiguousNativeSession>() => {
            eprintln!(
                "ai-memory: {error}, so its transcript was not imported; resume the session with its native selector to link it"
            );
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    // A linked session `own_native_session` rejected (not in this checkout,
    // or its native store could not be read) is no fallback either.
    let resolved = discovered.or_else(|| {
        server_status
            .and_then(|status| status.native_session_id.clone())
            .filter(|reported| Some(reported.as_str()) != linked)
    });
    Ok(resolved
        .as_deref()
        .map(|id| {
            NativeSessionIdentity::parse(id, sanitizer).map(NativeSessionIdentity::into_string)
        })
        .transpose()?)
}

async fn list_auto_sessions(
    home: &Path,
    cwd: &Path,
    env: &EffectiveChildEnv,
    opencode_harness: Option<ManagedHarness>,
) -> Result<Vec<AutoSessionCandidate>> {
    let mut found = Vec::new();
    let mut failures = Vec::new();
    for mut harness in AUTO_HARNESSES {
        if harness == ManagedHarness::OpenCode {
            let Some(resolved) = opencode_harness else {
                continue;
            };
            harness = resolved;
        }
        // Scan the store the launch would resume from. With a custom
        // `CLAUDE_CONFIG_DIR` (or another store override) the default store
        // holds none of this checkout's sessions, so scanning it either finds
        // nothing or picks a session the launched harness cannot see.
        let session_dir = match auto_session_dir(harness, home, cwd, env) {
            Ok(session_dir) => session_dir,
            Err(error) => {
                failures.push(format!("{}: {error}", harness.as_str()));
                continue;
            }
        };
        match list_native_sessions(harness, home, cwd, session_dir.as_deref(), 1).await {
            Ok(candidates) => found.extend(
                candidates
                    .into_iter()
                    .map(|session| AutoSessionCandidate { harness, session }),
            ),
            Err(error) => failures.push(format!("{}: {error}", harness.as_str())),
        }
    }
    if found.is_empty() && !failures.is_empty() {
        return Err(anyhow!(
            "could not inspect checkout-local sessions: {}",
            failures.join("; ")
        ));
    }
    for failure in failures {
        eprintln!("ai-memory: session scan skipped {failure}");
    }
    found.sort_by(|left, right| {
        right
            .session
            .updated_at
            .cmp(&left.session.updated_at)
            .then_with(|| left.harness.as_str().cmp(right.harness.as_str()))
    });
    Ok(found)
}

/// The native store a bare launch of `harness` resolves, the same way the
/// launch plan does: `--env`/`--env-file` first, then the process environment.
fn auto_session_dir(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    env: &EffectiveChildEnv,
) -> Result<Option<PathBuf>> {
    Ok(build_launch_plan_with_env_lookup(
        harness,
        None,
        Vec::new(),
        None,
        |name| env.get(name).map(OsStr::to_os_string),
        Some(LaunchRoots { home, cwd }),
    )?
    .session_dir)
}

/// The checkout's git-remote identity when `project` is the name it derives,
/// so the server can route the run by identity the way hook capture does. A
/// name from `--project` that differs from it routes by name, and so does any
/// name a marker's `project` key pinned: capture never reads the remote then.
fn remote_identity_naming(cwd: &Path, project: &str, marker_project: bool) -> Option<String> {
    if marker_project {
        return None;
    }
    crate::marker::discover_remote_identity(&cwd.to_string_lossy())
        .filter(|identity| {
            ai_memory_core::repository_identity::path_style_name(identity).as_deref()
                == Some(project)
        })
        .map(|identity| identity.identity)
}

fn unique_auto_agents(candidates: &[AutoSessionCandidate]) -> Vec<AgentKind> {
    let mut agents = Vec::new();
    for candidate in candidates {
        let agent = candidate.harness.agent_kind();
        if !agents.contains(&agent) {
            agents.push(agent);
        }
    }
    agents
}

fn automatic_harness_flavor(
    selected: ManagedHarness,
    provisional: ManagedHarness,
    linked_session_id: Option<&str>,
    opencode_harness: Option<ManagedHarness>,
) -> ManagedHarness {
    if selected == ManagedHarness::Kiro
        && provisional.agent_kind() == AgentKind::KiroCli
        && linked_session_id.is_none()
    {
        provisional
    } else if selected == ManagedHarness::OpenCode {
        opencode_harness.unwrap_or(selected)
    } else {
        selected
    }
}

fn filter_usable_auto_sessions(
    candidates: Vec<AutoSessionCandidate>,
    available: impl Fn(ManagedHarness) -> bool,
) -> Result<Vec<AutoSessionCandidate>> {
    let mut missing = Vec::new();
    let usable = candidates
        .into_iter()
        .filter(|candidate| {
            if available(candidate.harness) {
                true
            } else {
                missing.push(candidate.harness.executable());
                false
            }
        })
        .collect::<Vec<_>>();
    if usable.is_empty() && !missing.is_empty() {
        missing.sort_unstable();
        missing.dedup();
        return Err(anyhow!(
            "checkout-local sessions were found, but their harness executables are not available in the host PATH: {}",
            missing.join(", ")
        ));
    }
    Ok(usable)
}

fn no_auto_session_error() -> anyhow::Error {
    anyhow!(
        "no Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, or Kiro CLI session was found for this directory; start one explicitly with `ai-memory run claude`, `ai-memory run codex`, `ai-memory run opencode`, `ai-memory run pi`, `ai-memory run crush`, `ai-memory run kimi`, `ai-memory run command-code`, or `ai-memory run kiro`"
    )
}

fn resolve_kiro_harness(
    native_args: &[OsString],
    linked_session_id: Option<&str>,
    source_cursor: Option<&str>,
    fallback: ManagedHarness,
    home: &Path,
    cwd: &Path,
) -> Result<ManagedHarness> {
    if kiro_selects_v3_engine(native_args) {
        return Ok(ManagedHarness::KiroV3);
    }
    if kiro_selects_v2_engine(native_args) {
        return Ok(ManagedHarness::Kiro);
    }
    if kiro_selects_non_default_engine(native_args) {
        return Ok(ManagedHarness::Kiro);
    }

    let explicit_session_id = kiro_explicit_session_id(native_args);
    if let Some(session_id) = explicit_session_id.as_deref().or(linked_session_id) {
        let mut found = Vec::new();
        for harness in [ManagedHarness::Kiro, ManagedHarness::KiroV3] {
            let probe = build_launch_plan(harness, None, Vec::new(), None)?;
            if native_session_exists(harness, home, cwd, probe.session_dir.as_deref(), session_id)?
            {
                found.push(harness);
            }
        }
        match found.as_slice() {
            [harness] => return Ok(*harness),
            [_, _] => {
                return Err(anyhow!(
                    "Kiro session {} exists in both incompatible engine stores; use --fresh with an explicit --agent-engine v2 or --v3",
                    display_session_id(session_id)
                ));
            }
            _ => {}
        }
    }

    // An exact user selector wins over the workstream's prior cursor. If its
    // store is unavailable, keep Kiro's documented v2 default unless the user
    // also selected v3 explicitly above.
    if explicit_session_id.is_some() {
        return Ok(ManagedHarness::Kiro);
    }

    if let Some(harness) = source_cursor.and_then(kiro_harness_from_source_cursor) {
        return Ok(harness);
    }
    Ok(fallback)
}

fn remove_wrapper_yolo(args: &mut Vec<OsString>) -> bool {
    let before = args.len();
    args.retain(|arg| arg != OsStr::new("--yolo"));
    args.len() != before
}

fn remove_wrapper_true_yolo(args: &mut Vec<OsString>) -> bool {
    let before = args.len();
    args.retain(|arg| arg != OsStr::new("--true-yolo"));
    args.len() != before
}

/// The effective permission modes for a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct YoloModes {
    /// Map the harness's dangerous-mode option, with the `--yolo` warning and
    /// ai-jail offer.
    yolo: bool,
    /// Additionally apply Claude's `bypassPermissions` + residual-prompt
    /// silencing (only acted on for the Claude harness).
    claude_true_yolo: bool,
}

/// `--true-yolo` is a superset of `--yolo`: it requests yolo on every harness
/// (where, outside Claude, it is simply interchangeable with `--yolo`) plus the
/// Claude-only bypass, so passing both is redundant but harmless. The
/// `claude_true_yolo` config key only upgrades a launch that is already yolo —
/// it never turns an ordinary run into a permission-bypassing one without the
/// `--yolo` warning.
fn yolo_modes(yolo_flag: bool, true_yolo_flag: bool, config_true_yolo: bool) -> YoloModes {
    let yolo = yolo_flag || true_yolo_flag;
    YoloModes {
        yolo,
        claude_true_yolo: yolo && (true_yolo_flag || config_true_yolo),
    }
}

fn remove_wrapper_fresh(args: &mut Vec<OsString>) -> bool {
    let before = args.len();
    args.retain(|arg| arg != OsStr::new("--fresh"));
    args.len() != before
}

fn remove_wrapper_force_unlock(args: &mut Vec<OsString>) -> bool {
    let before = args.len();
    args.retain(|arg| arg != OsStr::new("--force-unlock"));
    args.len() != before
}

fn remove_wrapper_no_autowire(args: &mut Vec<OsString>) -> bool {
    let before = args.len();
    args.retain(|arg| arg != OsStr::new("--no-autowire"));
    args.len() != before
}

/// The error for a `--profile` name `config.toml` does not define: it names
/// the profiles that do exist, or, with none defined, prints a table to paste.
fn unknown_run_profile(run: &crate::config::RunSettings, name: &str) -> anyhow::Error {
    if !run.profiles.is_empty() {
        let known = run.profiles.keys().map(String::as_str).collect::<Vec<_>>();
        return anyhow!(
            "unknown run profile {name:?}; defined profiles: {}",
            known.join(", ")
        );
    }
    // Quoted when needed: a valid profile name may contain `.`, which a bare
    // TOML key would split into nested tables.
    let key = toml_edit::Key::new(name);
    anyhow!(
        "unknown run profile {name:?}; none are defined. Add to your config.toml:\n\n  \
         [run.profiles.{}.env]\n  CLAUDE_CONFIG_DIR = \"/absolute/path\"",
        key.display_repr()
    )
}

/// Merge `--env-file` lines with `--env` entries into the final key/value
/// list applied to the spawned harness, preserving file order but letting a
/// `--env` entry override a same-key `--env-file` line. Values are taken
/// literally; neither source is expanded or interpreted.
fn resolve_run_env(
    profile: Option<&crate::config::RunProfile>,
    env_file: Option<&Path>,
    env_args: &[(String, String)],
) -> Result<Vec<(String, String)>> {
    let mut merged: Vec<(String, String)> = Vec::new();
    if let Some(profile) = profile {
        for (key, value) in &profile.env {
            upsert_env(&mut merged, key.clone(), value.clone());
        }
    }
    if let Some(path) = env_file {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("reading --env-file {}", path.display()))?;
        for (line_number, line) in contents.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (key, value) = crate::cli::parse_env_kv(trimmed)
                .map_err(|error| anyhow!("{}:{}: {error}", path.display(), line_number + 1))?;
            upsert_env(&mut merged, key, value);
        }
    }
    for (key, value) in env_args {
        upsert_env(&mut merged, key.clone(), value.clone());
    }
    Ok(merged)
}

/// Insert or replace one entry in an ordered env list, keeping the position
/// of an existing key so `--env-file` order stays stable across overrides.
fn upsert_env(entries: &mut Vec<(String, String)>, key: String, value: String) {
    upsert_env_for_platform(entries, key, value, cfg!(windows));
}

fn upsert_env_for_platform(
    entries: &mut Vec<(String, String)>,
    key: String,
    value: String,
    windows: bool,
) {
    if let Some(existing) = entries.iter_mut().find(|(existing_key, _)| {
        environment_keys_equal(OsStr::new(existing_key), OsStr::new(&key), windows)
    }) {
        existing.0 = key;
        existing.1 = value;
    } else {
        entries.push((key, value));
    }
}

/// The environment auto-wire resolves install targets from: the launch's own
/// `--env` entries, adjusted where the child runs with something else. OMP
/// ranks `--profile` above `OMP_PROFILE`, so the flag is passed on through the
/// profile variables (see [`omp_profile_flag_env`], which reads the launch
/// environment `launch_env`).
fn autowire_env(
    harness: ManagedHarness,
    run_env: &[(String, String)],
    native_args: &[OsString],
    launch_env: &dyn Fn(&str) -> Option<OsString>,
) -> Vec<(String, String)> {
    let mut env = run_env.to_vec();
    let mut set = |name: &str, value: String| {
        env.retain(|(key, _)| key != name);
        env.push((name.to_string(), value));
    };
    if harness == ManagedHarness::Omp
        && let Some(profile) = omp_profile_flag(native_args).filter(|name| !name.is_empty())
    {
        for (name, value) in omp_profile_flag_env(&profile, launch_env) {
            set(&name, value);
        }
    }
    env
}

/// The launched harness's home variables (its store overrides, plus the
/// config home Crush's managed context is built from) that are set but blank
/// in the launch environment.
fn blank_home_overrides(
    harness: ManagedHarness,
    get: &dyn Fn(&str) -> Option<OsString>,
) -> Vec<&'static str> {
    let crush_config: &[&'static str] = match harness {
        ManagedHarness::Crush => &["CRUSH_GLOBAL_CONFIG", "XDG_CONFIG_HOME"],
        _ => &[],
    };
    store_override_vars(harness)
        .iter()
        .chain(crush_config)
        .copied()
        .filter(|name| {
            let value = get(name);
            value.is_some() && ai_memory_workstream::env_dir_override(value).is_none()
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn build_preflighted_launch_plan(
    harness: ManagedHarness,
    executable: Option<OsString>,
    native_args: Vec<OsString>,
    linked_session_id: Option<&str>,
    force_fresh: bool,
    home: &Path,
    cwd: &Path,
    env: &EffectiveChildEnv,
) -> Result<(LaunchPlan, Option<String>)> {
    if let Some(id) = linked_session_id {
        NativeSessionIdentity::parse(id, &Sanitizer::builtin())?;
    }
    let explicit_selector = has_native_session_selector(harness, &native_args);
    if force_fresh && explicit_selector {
        return Err(anyhow!(
            "--fresh cannot be combined with a native session, resume, continue, or fork selector"
        ));
    }
    let linked_session_id = if force_fresh { None } else { linked_session_id };
    let plan = build_launch_plan_with_env_lookup(
        harness,
        executable.clone(),
        native_args.clone(),
        linked_session_id,
        |name| env.get(name).map(OsStr::to_os_string),
        Some(LaunchRoots { home, cwd }),
    )?;
    let Some(linked_session_id) = linked_session_id else {
        return Ok((plan, None));
    };
    if explicit_selector || plan.mode != LaunchMode::Session {
        return Ok((plan, None));
    }
    match native_session_exists(
        harness,
        home,
        cwd,
        plan.session_dir.as_deref(),
        linked_session_id,
    ) {
        Ok(true) => Ok((plan, None)),
        Ok(false) => Ok((
            build_launch_plan_with_env_lookup(
                harness,
                executable,
                native_args,
                None,
                |name| env.get(name).map(OsStr::to_os_string),
                Some(LaunchRoots { home, cwd }),
            )?,
            Some(linked_session_id.to_string()),
        )),
        Err(error) => {
            eprintln!(
                "ai-memory: could not verify linked {} session {} ({error}); preserving native resume. Use --fresh to bypass it",
                harness.as_str(),
                display_session_id(linked_session_id)
            );
            Ok((plan, None))
        }
    }
}

/// The store a launch reads, absolute and normalized so the default store and
/// an override spelling the same directory (relative, trailing slash,
/// symlinked) compare equal. A relative override resolves against the checkout,
/// where the harness runs.
fn effective_store(
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
) -> PathBuf {
    let root = native_store_root(harness, home, session_dir);
    let absolute = if root.is_absolute() {
        root
    } else {
        cwd.join(root)
    };
    std::fs::canonicalize(&absolute).unwrap_or_else(|_| canonical_prefix(&clean_path(&absolute)))
}

/// Canonicalize the deepest existing ancestor and keep the missing tail, so a
/// store that does not exist yet still compares equal to the same directory
/// once it does (on macOS, `/var` vs `/private/var`).
fn canonical_prefix(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut existing = path;
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            return tail.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

fn remember_session_store(
    config: &Config,
    endpoint: &ServerEndpoint,
    harness: ManagedHarness,
    native_session_id: &str,
    store: &Path,
) {
    if let Err(error) = super::project_registry::record_session_store(
        config,
        endpoint,
        harness.as_str(),
        native_session_id,
        store,
    ) {
        eprintln!(
            "ai-memory: could not record the client-local session store ({error:#}); continuing the managed run"
        );
    }
}

/// Refuse to replace a linked session that was launched under a different
/// native store than this launch resolves.
///
/// Such a session most likely still exists in its own store; starting fresh
/// would repoint the workstream and orphan it. With no record (or the same
/// store) the session really is gone and replacing it stays the right call.
fn store_mismatch_error(
    harness: ManagedHarness,
    native_session_id: &str,
    recorded: Option<PathBuf>,
    current: &Path,
) -> Option<anyhow::Error> {
    let recorded = recorded?;
    if recorded == current {
        return None;
    }
    // Native flags and profiles move some stores too, not only the variables.
    let flags: &[&str] = match harness {
        ManagedHarness::Pi => &["--session-dir"],
        ManagedHarness::Omp => &["--session-dir", "--profile", "OMP_PROFILE"],
        ManagedHarness::Crush => &["--data-dir"],
        _ => &[],
    };
    let selectors = [store_override_vars(harness), flags].concat().join("/");
    let hint = if selectors.is_empty() {
        " from the original environment".to_owned()
    } else {
        format!(" with the same {selectors}")
    };
    Some(anyhow!(
        "linked {} session {} was launched under {} but this launch resolves {}; relaunch{hint} (or the same `ai-memory run --profile`) to resume it, or pass --fresh to start a new session",
        harness.as_str(),
        display_session_id(native_session_id),
        super::show::terminal_text(&recorded.to_string_lossy()),
        super::show::terminal_text(&current.to_string_lossy()),
    ))
}

fn ensure_executable_available(
    harness: ManagedHarness,
    executable: Option<&OsStr>,
    search: ExecutableSearch<'_>,
) -> Result<()> {
    let program = executable.unwrap_or_else(|| OsStr::new(harness.executable()));
    if executable_available(program, search) {
        return Ok(());
    }
    Err(anyhow!(
        "managed {} executable `{}` was not found in the host PATH; install it or pass `--executable`. Docker users should run `ai-memory upgrade` to refresh the host wrapper",
        harness.as_str(),
        program.to_string_lossy()
    ))
}

/// Whether this harness's default executable resolves through `PATH`.
///
/// Shared with `show`, so the picker offers exactly the harnesses that
/// [`ensure_executable_available`] would accept a moment later. Harnesses
/// reached only through `--executable` are not covered: the picker has no way
/// to ask for that path.
pub(super) fn harness_available(choice: RunHarnessChoice, search: ExecutableSearch<'_>) -> bool {
    executable_available(OsStr::new(managed_harness(choice).executable()), search)
}

fn executable_available(program: &OsStr, search: ExecutableSearch<'_>) -> bool {
    resolve_program(program, search).is_some()
}

/// The `claude attach` id for a linked Claude session that is still running in
/// the background, or `None` to keep the native `--resume`. The transcript is
/// checked first so a foreground session never costs a `claude agents` call;
/// any failure of that call (an older Claude, a wrapper executable, a timeout)
/// also keeps the resume.
async fn claude_background_attach_id(
    plan: &LaunchPlan,
    home: &Path,
    cwd: &Path,
    env: &EffectiveChildEnv,
    native_session_id: &str,
    search: ExecutableSearch<'_>,
) -> Option<String> {
    let background =
        claude_session_ran_in_background(home, cwd, plan.session_dir.as_deref(), native_session_id)
            .unwrap_or(false);
    if !background {
        return None;
    }
    let program =
        resolve_program(&plan.program, search).unwrap_or_else(|| plan.program.clone().into());
    let mut command = Command::new(&program);
    env.apply_to_tokio(&mut command);
    command
        .args(["agents", "--json", "--cwd"])
        .arg(cwd)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(CLAUDE_AGENTS_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    claude_live_background_attach_id(&output.stdout, native_session_id, cwd)
}

/// Resolve `program` to a concrete path the OS can actually start, or `None`
/// when nothing launchable matches.
///
/// The bare name is not enough on Windows. An npm-style install drops three
/// files next to each other — `opencode`, `opencode.cmd`, `opencode.ps1` — and
/// only the ones carrying a `PATHEXT` extension are launchable: `CreateProcess`
/// refuses the extension-less shell script, which exists for Git Bash. Probing
/// for mere existence therefore reported harnesses as present that then failed
/// to spawn with "program not found". Resolving to the concrete file keeps the
/// availability check and the launch agreeing on one answer, and lets the
/// launch use a path that works.
pub(super) fn resolve_program(
    program: &OsStr,
    search: ExecutableSearch<'_>,
) -> Option<std::path::PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return resolve_candidate(path, search.extensions);
    }
    search.path.and_then(|path_value| {
        std::env::split_paths(path_value)
            .find_map(|dir| resolve_candidate(&dir.join(path), search.extensions))
    })
}

/// Concrete launchable file for one candidate location.
fn resolve_candidate(
    path: &Path,
    executable_extensions: Option<&OsStr>,
) -> Option<std::path::PathBuf> {
    #[cfg(not(windows))]
    let _ = executable_extensions;
    #[cfg(windows)]
    {
        // An explicit extension is taken at face value; otherwise only a
        // PATHEXT match counts. Never the extension-less sibling.
        if path.extension().is_some() && executable_file(path) {
            return Some(path.to_path_buf());
        }
        let extensions = executable_extensions
            .and_then(OsStr::to_str)
            .unwrap_or(".COM;.EXE;.BAT;.CMD");
        extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| path.with_extension(extension.trim_start_matches('.')))
            .find(|candidate| executable_file(candidate))
    }
    #[cfg(not(windows))]
    {
        executable_file(path).then(|| path.to_path_buf())
    }
}

fn executable_file(path: &Path) -> bool {
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

fn user_supplied_grok_rules(native_args: &[OsString]) -> bool {
    native_args.iter().any(|arg| {
        arg.to_str().is_some_and(|value| {
            ["--rules", "--append-system-prompt"]
                .iter()
                .any(|name| value == *name || value.starts_with(&format!("{name}=")))
        })
    })
}

/// Grok appends `--rules` text to its session system prompt, so the packet is
/// delivered as an argument instead of a file. Acceptance happens only after
/// the child spawns, matching the Crush contract.
async fn fetch_grok_context(endpoint: &ServerEndpoint, run_path: &str) -> Result<Option<String>> {
    let response: ManagedRunContextResponse = post_json(
        endpoint,
        &format!("{run_path}/context"),
        &serde_json::json!({}),
    )
    .await
    .context("loading the managed context for Grok; the agent was not started")?;
    Ok(response.context)
}

async fn prepare_crush_context(
    endpoint: &ServerEndpoint,
    run_path: &str,
    source: &Path,
) -> Result<Option<tempfile::TempDir>> {
    let response: ManagedRunContextResponse = post_json(
        endpoint,
        &format!("{run_path}/context"),
        &serde_json::json!({}),
    )
    .await
    .context("loading the managed context for Crush; the agent was not started")?;
    let Some(context) = response.context else {
        return Ok(None);
    };

    write_crush_context_config(source, &context).map(Some)
}

/// The global `crush.json` the launched Crush reads. Crush takes a relative
/// `CRUSH_GLOBAL_CONFIG` from its working directory, and the generated config
/// dir (and its `crushrc`) must name the user's files absolutely because the
/// child reads them from elsewhere.
fn crush_context_source(
    home: &Path,
    cwd: &Path,
    get: impl Fn(&str) -> Option<OsString>,
) -> PathBuf {
    cwd.join(crush_global_config_path(home, get))
}

fn write_crush_context_config(source: &Path, context: &str) -> Result<tempfile::TempDir> {
    let temp = tempfile::Builder::new()
        .prefix("ai-memory-crush-")
        .tempdir()
        .context("creating the temporary Crush context directory")?;
    let context_path = temp.path().join("managed-workstream.md");
    write_private(&context_path, context.as_bytes())?;

    // Crush cleans the path (`filepath.Join`) before reading it, so a `..`
    // after a missing directory still reaches the file.
    let source = ai_memory_workstream::clean_path(source);
    let raw = if source.is_file() {
        std::fs::read(&source)
            .with_context(|| format!("reading Crush config {}", source.display()))?
    } else {
        Vec::new()
    };
    // Crush skips an empty file and reads `null` as unset; do the same rather
    // than refuse a config Crush itself accepts.
    let mut config = if raw.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice::<serde_json::Value>(&raw)
            .with_context(|| format!("parsing Crush config {}", source.display()))?
    };
    if config.is_null() {
        config = serde_json::json!({});
    }
    let root = config
        .as_object_mut()
        .context("Crush global config must be a JSON object")?;
    let options = root.entry("options").or_insert(serde_json::Value::Null);
    if options.is_null() {
        *options = serde_json::json!({});
    }
    let options = options
        .as_object_mut()
        .context("Crush global config `options` must be a JSON object")?;
    let paths = options
        .entry("global_context_paths")
        .or_insert(serde_json::Value::Null);
    if paths.is_null() {
        *paths = serde_json::json!([]);
    }
    let paths = paths
        .as_array_mut()
        .context("Crush `options.global_context_paths` must be an array")?;
    // Crush fills an empty list with `CRUSH.md` beside its global config and
    // `AGENTS.md` one level up, but only while the list is empty, and it would
    // resolve them against this temp dir. Seed the user's own ones first so
    // the packet does not replace them.
    if paths.is_empty()
        && let Some(config_dir) = source.parent()
    {
        paths.push(serde_json::Value::String(
            config_dir.join("CRUSH.md").to_string_lossy().into_owned(),
        ));
        if let Some(parent) = config_dir.parent() {
            paths.push(serde_json::Value::String(
                parent.join("AGENTS.md").to_string_lossy().into_owned(),
            ));
        }
    }
    let context_path = context_path.to_string_lossy().into_owned();
    if !paths
        .iter()
        .any(|value| value.as_str() == Some(&context_path))
    {
        paths.push(serde_json::Value::String(context_path));
    }
    let rendered = serde_json::to_vec_pretty(&config).context("rendering Crush config")?;
    write_private(&temp.path().join("crush.json"), &rendered)?;
    // Crush also runs the `crushrc` beside its global config, and with the
    // config dir moved here it would look for one in this dir. Source the
    // user's own from the directory Crush runs it in, so its relative paths
    // and `source` lines still resolve. Its settings merge over the JSON
    // above and lists concatenate, so the packet stays loaded. Unlike Crush,
    // the default context files above are seeded even when the script adds
    // paths of its own.
    if let Some(config_dir) = source.parent() {
        let crushrc = config_dir.join("crushrc");
        if crushrc.is_file() {
            let quote =
                |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"));
            let wrapper = format!("cd {} && source {}\n", quote(config_dir), quote(&crushrc));
            write_private(&temp.path().join("crushrc"), wrapper.as_bytes())?;
        }
    }
    Ok(temp)
}

fn write_private(path: &Path, content: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(content)
        .with_context(|| format!("writing {}", path.display()))
}

pub(crate) fn native_home(config: &Config) -> Option<PathBuf> {
    config
        .home_dir
        .as_deref()
        .map(PathBuf::from)
        .or_else(path_util::home_dir)
}

async fn choose_native_session_interactive(
    harness: ManagedHarness,
    workstream_name: String,
    candidates: Vec<NativeSessionCandidate>,
    endpoint: &ServerEndpoint,
    run_path: &str,
    interrupted: &CancellationToken,
) -> Result<io::Result<Option<String>>> {
    let chooser = tokio::task::spawn_blocking(move || {
        let stdin = io::stdin();
        let mut stderr = io::stderr();
        choose_native_session(
            harness,
            &workstream_name,
            &candidates,
            &mut stdin.lock(),
            // Keep stderr available to report cancellation while stdin is blocked.
            &mut stderr,
            SystemTime::now(),
        )
    });
    wait_for_native_session_choice(chooser, endpoint, run_path, interrupted).await
}

async fn wait_for_native_session_choice(
    mut chooser: tokio::task::JoinHandle<io::Result<Option<String>>>,
    endpoint: &ServerEndpoint,
    run_path: &str,
    interrupted: &CancellationToken,
) -> Result<io::Result<Option<String>>> {
    tokio::select! {
        biased;
        _ = interrupted.cancelled() => {
            // A running stdin read cannot be aborted. The CLI runtime's bounded
            // shutdown lets the process exit after the caller cancels the lease.
            chooser.abort();
            Err(anyhow!("managed run interrupted before the agent started"))
        }
        result = async {
            let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            heartbeat.tick().await;
            let mut heartbeat_health = HeartbeatHealth::default();
            let selection = loop {
                tokio::select! {
                    result = &mut chooser => {
                        break result.context("waiting for the native session choice")?;
                    }
                    _ = heartbeat.tick() => {
                        let _ = send_managed_heartbeat(endpoint, run_path, &mut heartbeat_health).await;
                    }
                }
            };
            send_managed_heartbeat(endpoint, run_path, &mut heartbeat_health)
                .await
                .context(
                    "renewing the managed workstream after session selection; the agent was not started",
                )?;
            Ok(selection)
        } => result,
    }
}

fn choose_native_session(
    harness: ManagedHarness,
    workstream_name: &str,
    candidates: &[NativeSessionCandidate],
    input: &mut impl io::BufRead,
    output: &mut impl io::Write,
    now: SystemTime,
) -> io::Result<Option<String>> {
    writeln!(
        output,
        "ai-memory: no {} session is linked to workstream '{}'.",
        harness.as_str(),
        workstream_name
    )?;
    writeln!(output, "Previous sessions for this checkout:")?;
    for (index, candidate) in candidates.iter().enumerate() {
        writeln!(
            output,
            "  {}) {} (updated {})",
            index + 1,
            display_session_id(&candidate.native_session_id),
            session_age(candidate.updated_at, now)
        )?;
    }
    writeln!(output, "  0) Start a new {} session", harness.as_str())?;

    loop {
        write!(output, "Select [1]: ")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            writeln!(output)?;
            return Ok(None);
        }
        let choice = line.trim();
        if choice.is_empty() {
            return Ok(Some(candidates[0].native_session_id.clone()));
        }
        if matches!(choice.to_ascii_lowercase().as_str(), "0" | "n" | "new") {
            return Ok(None);
        }
        if let Ok(index) = choice.parse::<usize>()
            && let Some(candidate) = index.checked_sub(1).and_then(|i| candidates.get(i))
        {
            return Ok(Some(candidate.native_session_id.clone()));
        }
        writeln!(output, "Enter 0 through {}.", candidates.len())?;
    }
}

fn display_session_id(value: &str) -> String {
    const MAX_CHARS: usize = 64;
    let mut output = value.chars().take(MAX_CHARS).collect::<String>();
    if value.chars().count() > MAX_CHARS {
        output.push_str("...");
    }
    output
}

/// Age of a native session, rendered the way every other read-only listing
/// renders one. Gains a "months" tier over the previous day-capped form, so a
/// long-idle session reads as `2 months ago` here and in `show` / `workstreams`
/// / `handoffs` rather than `74 days ago` in this one command.
fn session_age(updated_at: SystemTime, now: SystemTime) -> String {
    let secs = now.duration_since(updated_at).unwrap_or_default().as_secs();
    super::humanize_age_secs(i64::try_from(secs).unwrap_or(i64::MAX))
}

async fn export_exact_after_run(
    harness: ManagedHarness,
    home: &std::path::Path,
    cwd: &std::path::Path,
    session_dir: Option<&std::path::Path>,
    native_session_id: &str,
    source_cursor: Option<&str>,
) -> Result<ExportedTranscript> {
    wait_for_transcript_flush(harness, home, cwd, session_dir, native_session_id).await?;
    export_transcript(
        harness,
        home,
        cwd,
        session_dir,
        native_session_id,
        source_cursor,
    )
    .await
}

async fn export_after_flush(
    harness: ManagedHarness,
    home: &std::path::Path,
    cwd: &std::path::Path,
    session_dir: Option<&std::path::Path>,
    native_session_id: Option<&str>,
    source_cursor: Option<&str>,
) -> ExportedTranscript {
    let Some(native_session_id) = native_session_id else {
        return ExportedTranscript {
            losses: vec![
                "native session id could not be discovered; transcript was not imported".into(),
            ],
            ..ExportedTranscript::default()
        };
    };
    if let Err(error) =
        wait_for_transcript_flush(harness, home, cwd, session_dir, native_session_id).await
    {
        eprintln!("ai-memory: transcript flush check failed: {error}");
    }
    match export_transcript(
        harness,
        home,
        cwd,
        session_dir,
        native_session_id,
        source_cursor,
    )
    .await
    {
        Ok(export) => export,
        Err(error) => ExportedTranscript {
            native_session_id: native_session_id.to_string(),
            source_cursor: source_cursor.map(str::to_string),
            losses: vec![format!("native transcript import failed: {error}")],
            events: Vec::new(),
        },
    }
}

pub(crate) async fn import_batches(
    endpoint: &ServerEndpoint,
    run_path: &str,
    transcript: ExportedTranscript,
    checkpoint: ai_memory_core::WorkstreamCheckpoint,
    exit_code: Option<i32>,
    sanitizer: &Sanitizer,
) -> Result<usize> {
    let native_identity = nonempty_session(&transcript.native_session_id)
        .map(|id| NativeSessionIdentity::parse(&id, sanitizer))
        .transpose()?;
    for event in &transcript.events {
        NativeSessionIdentity::parse(&event.native_session_id, sanitizer)?;
    }
    let mut imported = 0;
    let mut batches = event_batches(transcript.events).into_iter().peekable();
    while let Some(batch) = batches.next() {
        let complete = batches.peek().is_none();
        let request = FinishManagedRunRequest {
            native_session_id: native_identity.as_ref().map(|id| id.as_str().to_owned()),
            source_cursor: complete.then(|| transcript.source_cursor.clone()).flatten(),
            events: batch,
            complete,
            checkpoint: checkpoint.clone(),
            losses: if complete {
                transcript.losses.clone()
            } else {
                Vec::new()
            },
            exit_code: complete.then_some(exit_code).flatten(),
        };
        imported += finish_with_retry(endpoint, run_path, &request)
            .await?
            .imported_events;
    }
    Ok(imported)
}

fn event_batches(
    events: Vec<ai_memory_core::NewWorkstreamEvent>,
) -> Vec<Vec<ai_memory_core::NewWorkstreamEvent>> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let mut bytes = 0_usize;
    for event in events {
        let event_bytes = serde_json::to_vec(&event).map_or(IMPORT_BATCH_BYTES, |raw| raw.len());
        if !batch.is_empty()
            && (batch.len() >= IMPORT_BATCH_EVENTS
                || bytes.saturating_add(event_bytes) > IMPORT_BATCH_BYTES)
        {
            batches.push(std::mem::take(&mut batch));
            bytes = 0;
        }
        bytes = bytes.saturating_add(event_bytes);
        batch.push(event);
    }
    batches.push(batch);
    batches
}

async fn finish_with_retry(
    endpoint: &ServerEndpoint,
    run_path: &str,
    request: &FinishManagedRunRequest,
) -> Result<FinishManagedRunResponse> {
    let path = format!("{run_path}/finish");
    let mut last_error = None;
    for attempt in 0..3 {
        match post_json(endpoint, &path, request).await {
            Ok(response) => return Ok(response),
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
    }
    Err(last_error.unwrap_or_else(|| anyhow!("managed finish failed")))
        .context("persisting the managed transcript; the native process has already exited")
}

struct PrepareRunOutcome {
    result: Result<PrepareManagedRunResponse>,
    manifest_warning: Option<ai_memory_core::repository_identity::ManifestWarning>,
}

impl PrepareRunOutcome {
    fn from_result(result: Result<PrepareManagedRunResponse>) -> Self {
        let manifest_warning = match &result {
            Ok(response) => response.manifest_warning.clone(),
            Err(error) => server_manifest_warning(error),
        };
        Self {
            result,
            manifest_warning,
        }
    }

    fn prefer_warning(
        mut self,
        first: Option<ai_memory_core::repository_identity::ManifestWarning>,
    ) -> Self {
        self.manifest_warning = first.or(self.manifest_warning);
        self
    }
}

fn complete_prepare_run(
    outcome: PrepareRunOutcome,
    output: &mut impl io::Write,
) -> Result<PrepareManagedRunResponse> {
    match outcome.result {
        Ok(mut response) => {
            response.manifest_warning = outcome.manifest_warning.or(response.manifest_warning);
            Ok(response)
        }
        Err(error) => {
            if let Some(warning) = outcome.manifest_warning {
                let _ = writeln!(output, "ai-memory: warning: {warning}");
            }
            Err(error)
        }
    }
}

async fn prepare_managed_run(
    endpoint: &ServerEndpoint,
    request: &PrepareManagedRunRequest,
    interactive: bool,
    interrupted: &CancellationToken,
) -> Result<PrepareManagedRunResponse> {
    if request.force_unlock {
        let result = match post_json(endpoint, "/workstream/runs", request).await {
            Err(error) if is_active_workstream_conflict(&error) => Err(error.context(
                "--force-unlock was refused: the active lease belongs to another operator, or \
                 the server does not support forced lease recovery",
            )),
            other => other,
        };
        return complete_prepare_run(PrepareRunOutcome::from_result(result), &mut io::stderr());
    }
    let outcome = prepare_managed_run_with_retry(
        endpoint,
        request,
        PREPARE_BUSY_RETRY_WINDOW,
        PREPARE_BUSY_RETRY_INTERVAL,
        true,
    )
    .await;
    let outcome = if interactive && outcome.result.is_err() {
        // Scripts, hooks, and CI keep the short window: they must never hang
        // silently for up to a full lease.
        wait_out_held_lease(
            endpoint,
            request,
            outcome,
            interrupted,
            HELD_LEASE_EXPIRY_SLACK,
            PREPARE_BUSY_RETRY_WINDOW,
        )
        .await
    } else {
        outcome
    };
    complete_prepare_run(outcome, &mut io::stderr())
}

/// After the quick retry window, an interactive launch waits out a lease left
/// behind by a launcher that could not release it (killed, terminal closed,
/// sandbox torn down) instead of failing: the 409 names the lease's expiry, so
/// wait for it to lapse and retry. A lease renewed meanwhile belongs to a
/// launcher that is still running; that is reported, never waited on or taken
/// over (the server's busy check stays the only arbiter of ownership).
async fn wait_out_held_lease(
    endpoint: &ServerEndpoint,
    request: &PrepareManagedRunRequest,
    outcome: PrepareRunOutcome,
    interrupted: &CancellationToken,
    slack: Duration,
    retry_window: Duration,
) -> PrepareRunOutcome {
    let PrepareRunOutcome {
        result,
        manifest_warning,
    } = outcome;
    let Err(error) = result else {
        return PrepareRunOutcome {
            result,
            manifest_warning,
        };
    };
    let Some(held) = held_lease(&error) else {
        return PrepareRunOutcome {
            result: Err(error),
            manifest_warning,
        };
    };
    let Some(wait) = held_lease_wait(held.expires, jiff::Timestamp::now(), slack) else {
        return PrepareRunOutcome {
            result: Err(error),
            manifest_warning,
        };
    };
    eprintln!(
        "ai-memory: the workstream is held by {} until {} — usually a launcher that exited \
         without releasing it. Waiting {}s for that lease to lapse (Ctrl-C to abort; \
         `--new <name>` starts a separate workstream).",
        held.owner,
        held.expires,
        wait.as_secs_f64().ceil()
    );
    tokio::select! {
        biased;
        () = interrupted.cancelled() => {
            return PrepareRunOutcome {
                result: Err(error.context("interrupted while waiting for the workstream lease to lapse")),
                manifest_warning,
            };
        }
        () = tokio::time::sleep(wait) => {}
    }
    let retry = prepare_managed_run_with_retry(
        endpoint,
        request,
        retry_window,
        PREPARE_BUSY_RETRY_INTERVAL,
        false,
    )
    .await
    .prefer_warning(manifest_warning);
    match retry.result {
        Err(error) if held_lease(&error).is_some() => PrepareRunOutcome {
            result: Err(error.context(
                "the workstream is still held: its owner renewed the lease, so another launcher \
                 is running there; stop it, or pass `--new <name>` for a separate workstream",
            )),
            manifest_warning: retry.manifest_warning,
        },
        result => PrepareRunOutcome {
            result,
            manifest_warning: retry.manifest_warning,
        },
    }
}

/// The owner and expiry a busy `POST /workstream/runs` reports.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HeldLease {
    owner: String,
    expires: jiff::Timestamp,
}

/// Parse the store's `workstream is already active: owned by <owner> until
/// <rfc3339>` message. `None` for any other shape (e.g. an older server).
fn parse_held_lease(message: &str) -> Option<HeldLease> {
    let rest = message.strip_prefix("workstream is already active: owned by ")?;
    let (owner, until) = rest.rsplit_once(" until ")?;
    Some(HeldLease {
        owner: owner.to_string(),
        expires: until.trim().parse().ok()?,
    })
}

fn held_lease(error: &anyhow::Error) -> Option<HeldLease> {
    parse_held_lease(&active_workstream_conflict_message(error)?)
}

/// How long to wait for a held lease to lapse, or `None` when it expires
/// further out than [`HELD_LEASE_MAX_WAIT`] (a renewing, live owner — or a
/// badly skewed clock). An already-lapsed lease waits only the slack.
fn held_lease_wait(
    expires: jiff::Timestamp,
    now: jiff::Timestamp,
    slack: Duration,
) -> Option<Duration> {
    let remaining = Duration::try_from(expires.duration_since(now)).unwrap_or(Duration::ZERO);
    (remaining <= HELD_LEASE_MAX_WAIT).then(|| remaining + slack)
}

async fn prepare_managed_run_with_retry(
    endpoint: &ServerEndpoint,
    request: &PrepareManagedRunRequest,
    retry_window: Duration,
    retry_interval: Duration,
    announce_wait: bool,
) -> PrepareRunOutcome {
    let deadline = tokio::time::Instant::now() + retry_window;
    let mut reported_wait = !announce_wait;
    let mut manifest_warning = None;
    loop {
        match post_json(endpoint, "/workstream/runs", request).await {
            Ok(response) => {
                return PrepareRunOutcome::from_result(Ok(response))
                    .prefer_warning(manifest_warning);
            }
            Err(error)
                if is_active_workstream_conflict(&error)
                    && tokio::time::Instant::now() < deadline =>
            {
                if manifest_warning.is_none() {
                    manifest_warning = server_manifest_warning(&error);
                }
                if !reported_wait {
                    eprintln!(
                        "ai-memory: another launcher owns this workstream; waiting briefly in case it is finalizing"
                    );
                    reported_wait = true;
                }
                tokio::time::sleep(retry_interval).await;
            }
            Err(error) => {
                return PrepareRunOutcome::from_result(Err(error)).prefer_warning(manifest_warning);
            }
        }
    }
}

fn is_active_workstream_conflict(error: &anyhow::Error) -> bool {
    active_workstream_conflict_message(error).is_some()
}

fn server_manifest_warning(
    error: &anyhow::Error,
) -> Option<ai_memory_core::repository_identity::ManifestWarning> {
    let response = error.downcast_ref::<ServerResponseError>()?;
    serde_json::from_str::<ai_memory_core::repository_identity::ManifestWarningContext>(
        response.body(),
    )
    .ok()?
    .manifest_warning
}

fn active_workstream_conflict_message(error: &anyhow::Error) -> Option<String> {
    let response = error.downcast_ref::<ServerResponseError>()?;
    if response.status() != reqwest::StatusCode::CONFLICT {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(response.body())
        .ok()?
        .get("error")?
        .as_str()
        .filter(|message| message.starts_with("workstream is already active:"))
        .map(str::to_owned)
}

async fn post_empty_with_retry(endpoint: &ServerEndpoint, path: &str, label: &str) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..3 {
        match post_empty(endpoint, path).await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(250 * (attempt + 1))).await;
    }
    Err(last_error.unwrap_or_else(|| anyhow!("request failed"))).context(label.to_string())
}

fn nonempty_session(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn lease_owner() -> String {
    let host = sysinfo::System::host_name()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .filter(|value| !value.trim().is_empty());
    lease_owner_label(host.as_deref(), std::process::id())
}

fn lease_owner_label(host: Option<&str>, process_id: u32) -> String {
    format!("{}:{process_id}", host.unwrap_or("localhost"))
}

const fn managed_harness(choice: RunHarnessChoice) -> ManagedHarness {
    match choice {
        RunHarnessChoice::Claude => ManagedHarness::Claude,
        RunHarnessChoice::Codex => ManagedHarness::Codex,
        RunHarnessChoice::OpenCode => ManagedHarness::OpenCode,
        RunHarnessChoice::OpenCode2 => ManagedHarness::OpenCode2,
        RunHarnessChoice::Pi => ManagedHarness::Pi,
        RunHarnessChoice::Crush => ManagedHarness::Crush,
        RunHarnessChoice::Omp => ManagedHarness::Omp,
        RunHarnessChoice::Kimi => ManagedHarness::Kimi,
        RunHarnessChoice::CommandCode => ManagedHarness::CommandCode,
        RunHarnessChoice::Kiro => ManagedHarness::Kiro,
        RunHarnessChoice::Grok => ManagedHarness::Grok,
        RunHarnessChoice::Antigravity => ManagedHarness::Antigravity,
        RunHarnessChoice::Copilot => ManagedHarness::Copilot,
    }
}

fn managed_harness_for_args(choice: RunHarnessChoice, native_args: &[OsString]) -> ManagedHarness {
    let harness = managed_harness(choice);
    if harness == ManagedHarness::Kiro && kiro_selects_v3_engine(native_args) {
        ManagedHarness::KiroV3
    } else {
        harness
    }
}

const fn managed_harness_from_agent(agent: AgentKind) -> Option<ManagedHarness> {
    match agent {
        AgentKind::ClaudeCode => Some(ManagedHarness::Claude),
        AgentKind::Codex => Some(ManagedHarness::Codex),
        AgentKind::OpenCode => Some(ManagedHarness::OpenCode),
        AgentKind::Pi => Some(ManagedHarness::Pi),
        AgentKind::Crush => Some(ManagedHarness::Crush),
        AgentKind::KimiCode => Some(ManagedHarness::Kimi),
        AgentKind::CommandCode => Some(ManagedHarness::CommandCode),
        AgentKind::KiroCli => Some(ManagedHarness::Kiro),
        AgentKind::Grok => Some(ManagedHarness::Grok),
        AgentKind::AntigravityCli => Some(ManagedHarness::Antigravity),
        AgentKind::CopilotCli => Some(ManagedHarness::Copilot),
        _ => None,
    }
}

/// Parse the exact harness names the recovery journal writes
/// (`ManagedHarness::as_str`). The user-facing `from_name` table deliberately
/// has no "kiro-v3" spelling (no one types that), but the journal must round
/// trip its own exact value, engine flavor included.
pub(crate) fn harness_from_journal_name(name: &str) -> Option<ManagedHarness> {
    match name {
        "claude" => Some(ManagedHarness::Claude),
        "codex" => Some(ManagedHarness::Codex),
        "opencode" => Some(ManagedHarness::OpenCode),
        "opencode2" => Some(ManagedHarness::OpenCode2),
        "pi" => Some(ManagedHarness::Pi),
        "crush" => Some(ManagedHarness::Crush),
        "omp" => Some(ManagedHarness::Omp),
        "kimi" => Some(ManagedHarness::Kimi),
        "command-code" => Some(ManagedHarness::CommandCode),
        "kiro" => Some(ManagedHarness::Kiro),
        "kiro-v3" => Some(ManagedHarness::KiroV3),
        "grok" => Some(ManagedHarness::Grok),
        "antigravity" => Some(ManagedHarness::Antigravity),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::io::Cursor;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ai_memory_core::{ManagedRunId, WorkstreamId};
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse as _;
    use axum::routing::post;
    use clap::Parser as _;

    use super::*;
    use crate::cli::{Cli, Command as CliCommand};
    #[cfg(unix)]
    use crate::commands::run_autowire::WireOverrides;

    fn plan(warn: bool, mode: JailMode) -> JailPlan {
        JailPlan { warn, mode }
    }

    #[test]
    fn run_sends_the_remote_identity_only_for_the_name_it_derives() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("new-space-game");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "-q"][..],
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:victorcesc/unknown-system.git",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        assert_eq!(
            remote_identity_naming(&repo, "victorcesc-unknown-system", false).as_deref(),
            Some("github.com/victorcesc/unknown-system")
        );
        // A folder-name, `--project`, or marker name routes by name alone.
        assert_eq!(remote_identity_naming(&repo, "new-space-game", false), None);
        assert_eq!(
            remote_identity_naming(tmp.path(), "victorcesc-unknown-system", false),
            None
        );
    }

    /// Hook capture reads no remote once a marker's `project` key names the
    /// project, so the run must not send one either, even when the pinned
    /// name happens to equal the remote-derived one.
    #[test]
    fn run_sends_no_remote_identity_for_a_marker_pinned_project() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("new-space-game");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "-q"][..],
            &[
                "remote",
                "add",
                "origin",
                "git@github.com:victorcesc/unknown-system.git",
            ],
        ] {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(&repo)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let config = Config {
            runtime_env: crate::config::RuntimeEnv::with_host_cwd_for_tests(repo.to_str().unwrap()),
            ..Config::default()
        };
        let identity_for = |marker: &str| {
            std::fs::write(repo.join(".ai-memory.toml"), marker).unwrap();
            let (_, project, marker_project) =
                resolve_scope_noting_marker_project(&config, None, None).unwrap();
            assert_eq!(project, "victorcesc-unknown-system");
            remote_identity_naming(&repo, &project, marker_project)
        };

        assert_eq!(
            identity_for("workspace = \"acme\"\nproject = \"victorcesc-unknown-system\"\n"),
            None
        );
        // A marker that pins only the workspace leaves the name to the remote.
        assert_eq!(
            identity_for("workspace = \"acme\"\n").as_deref(),
            Some("github.com/victorcesc/unknown-system")
        );
    }

    /// The full decision table: request × jailed × yolo × interactive ×
    /// usable × project `.ai-jail`. `interactive` stands for "stdin and stderr
    /// are both TTYs".
    #[test]
    fn jail_decision_table() {
        use JailMode::{Explicit, Offer, Stay};
        let unspecified = JailRequest::Unspecified;
        let never = JailRequest::Never;
        let defaults = JailRequest::Explicit(String::new());
        let listed = JailRequest::Explicit("gpu,ssh".to_owned());
        let requests = [&unspecified, &never, &defaults, &listed];

        // Already inside ai-jail: nothing applies, not even an unusable --jail.
        for request in requests {
            for (yolo, interactive, usable, project) in [
                (true, true, true, false),
                (true, true, true, true),
                (true, true, false, false),
                (false, false, false, true),
                (true, false, true, false),
            ] {
                assert_eq!(
                    jail_decision(request, true, yolo, interactive, usable, project).unwrap(),
                    plan(false, Stay),
                    "{request:?} inside the jail"
                );
            }
        }

        for project in [false, true] {
            let decide = |request: &JailRequest, yolo, interactive, usable| {
                jail_decision(request, false, yolo, interactive, usable, project).unwrap()
            };
            // Neither flag: the offer needs yolo, a TTY, and a usable ai-jail;
            // the warning needs yolo and a TTY. A project `.ai-jail` replaces
            // the checklist and nothing else.
            assert_eq!(
                decide(&unspecified, true, true, true),
                plan(
                    true,
                    Offer {
                        checklist: !project
                    }
                ),
                "project .ai-jail = {project}"
            );
            assert_eq!(decide(&unspecified, true, true, false), plan(true, Stay));
            assert_eq!(decide(&unspecified, true, false, true), plan(false, Stay));
            assert_eq!(decide(&unspecified, false, true, true), plan(false, Stay));

            // --no-jail: never jail, but the --yolo warning still shows.
            assert_eq!(decide(&never, true, true, true), plan(true, Stay));
            assert_eq!(decide(&never, true, false, true), plan(false, Stay));
            assert_eq!(decide(&never, false, true, true), plan(false, Stay));

            // --jail / --jail=LIST: re-exec without the question, with or
            // without --yolo, interactive or not; the warning still shows for
            // yolo + TTY. (What a project file does to the toggles is
            // `jail_checklist`'s job: nothing pre-checked.)
            for (request, list) in [(&defaults, ""), (&listed, "gpu,ssh")] {
                let explicit = || Explicit(list.to_owned());
                assert_eq!(decide(request, true, true, true), plan(true, explicit()));
                assert_eq!(decide(request, true, false, true), plan(false, explicit()));
                assert_eq!(decide(request, false, true, true), plan(false, explicit()));
                assert_eq!(decide(request, false, false, true), plan(false, explicit()));
                for (yolo, interactive) in [(true, true), (false, false)] {
                    let error = jail_decision(request, false, yolo, interactive, false, project)
                        .unwrap_err();
                    assert!(
                        error.to_string().contains("ai-jail is not usable"),
                        "an unusable ai-jail must fail, never run unjailed: {error}"
                    );
                }
            }
        }
    }

    #[test]
    fn jail_request_merges_leading_and_trailing_flags() {
        let none = TrailingJail::default();
        assert_eq!(
            jail_request(None, false, none.clone()).unwrap(),
            JailRequest::Unspecified
        );
        assert_eq!(
            jail_request(None, true, none.clone()).unwrap(),
            JailRequest::Never
        );
        assert_eq!(
            jail_request(Some(String::new()), false, none.clone()).unwrap(),
            JailRequest::Explicit(String::new())
        );
        let trailing = TrailingJail {
            jail: Some("gpu".to_owned()),
            no_jail: false,
        };
        assert_eq!(
            jail_request(Some("aws".to_owned()), false, trailing.clone()).unwrap(),
            JailRequest::Explicit("gpu".to_owned()),
            "the later (trailing) --jail wins"
        );
        assert!(jail_request(None, true, trailing).is_err());
        let trailing_no = TrailingJail {
            jail: None,
            no_jail: true,
        };
        let error = jail_request(Some(String::new()), false, trailing_no).unwrap_err();
        assert!(error.to_string().contains("cannot be used together"));
    }

    fn parse_run(argv: &[&str]) -> RunArgs {
        let CliCommand::Run(args) = crate::cli::try_parse_from(argv).unwrap().command else {
            panic!("expected run command");
        };
        args
    }

    /// Bare `--jail` must not eat the harness name, and every spelling must
    /// be a wrapper flag whether clap sees it (before the harness) or it lands
    /// in `native_args` (after a native argument) — never reaching the harness.
    #[test]
    fn jail_flags_are_wrapper_flags_in_either_position() {
        let bare = parse_run(&["ai-memory", "run", "--jail", "claude"]);
        assert_eq!(bare.jail.as_deref(), Some(""));
        assert!(matches!(bare.harness, Some(RunHarnessChoice::Claude)));
        assert!(bare.native_args.is_empty());

        let listed = parse_run(&["ai-memory", "run", "--jail=gpu,ssh", "claude"]);
        assert_eq!(listed.jail.as_deref(), Some("gpu,ssh"));
        assert!(matches!(listed.harness, Some(RunHarnessChoice::Claude)));

        let never = parse_run(&["ai-memory", "run", "--no-jail", "claude"]);
        assert!(never.no_jail && never.jail.is_none());

        assert!(
            Cli::try_parse_from(["ai-memory", "run", "--jail", "--no-jail", "claude"]).is_err(),
            "clap rejects both flags together"
        );

        let swallowed = parse_run(&[
            "ai-memory",
            "run",
            "claude",
            "--model",
            "opus",
            "--jail=github,no-mise",
            "--worktree",
        ]);
        assert!(
            swallowed.jail.is_none(),
            "clap leaves it in the native argv"
        );
        let mut native = swallowed.native_args;
        assert_eq!(
            remove_wrapper_jail(&mut native),
            TrailingJail {
                jail: Some("github,no-mise".to_owned()),
                no_jail: false
            }
        );
        assert_eq!(
            native,
            ["--model", "opus", "--worktree"].map(OsString::from),
            "only the wrapper flag is stripped; the harness's own --worktree stays"
        );

        for (flag, expected) in [
            (
                "--jail",
                TrailingJail {
                    jail: Some(String::new()),
                    no_jail: false,
                },
            ),
            (
                "--jail=",
                TrailingJail {
                    jail: Some(String::new()),
                    no_jail: false,
                },
            ),
            (
                "--no-jail",
                TrailingJail {
                    jail: None,
                    no_jail: true,
                },
            ),
        ] {
            let mut native =
                parse_run(&["ai-memory", "run", "claude", "--model", "opus", flag]).native_args;
            assert_eq!(remove_wrapper_jail(&mut native), expected, "{flag}");
            assert_eq!(native, ["--model", "opus"].map(OsString::from), "{flag}");
        }
    }

    fn checklist_items() -> Vec<JailChecklistItem> {
        ["github", "aws", "ssh", "docker"]
            .into_iter()
            .map(|stem| JailChecklistItem {
                toggle: jail_toggle(stem).unwrap(),
                checked: matches!(stem, "github" | "aws"),
            })
            .collect()
    }

    fn run_checklist(input: &str) -> (io::Result<Vec<String>>, String) {
        let mut output = Vec::new();
        let result = read_jail_checklist(
            &checklist_items(),
            &mut Cursor::new(input.as_bytes().to_vec()),
            &mut output,
        )
        .map(|choices| choices.iter().map(JailToggleChoice::flag).collect());
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn checklist_enter_and_eof_accept_the_smart_defaults() {
        let (enter, printed) = run_checklist("\n");
        assert_eq!(
            enter.unwrap(),
            ["--github", "--aws", "--no-ssh", "--no-docker"],
            "every row as seen: unchecked rows are forced off"
        );
        assert!(printed.contains("Enable in the jail (Enter = as marked"));
        assert!(printed.contains("[x] 1) GitHub CLI credentials"));
        assert!(printed.contains("[x] 2) AWS credentials"));
        assert!(printed.contains("[ ] 3) SSH keys + agent"));
        assert!(printed.contains("[ ] 4) Docker socket"));
        assert!(printed.contains("⚠ grants host root"));
        let (eof, _) = run_checklist("");
        assert_eq!(
            eof.unwrap(),
            ["--github", "--aws", "--no-ssh", "--no-docker"]
        );
    }

    #[test]
    fn checklist_numbers_flip_rows_and_all_none_set_every_row() {
        let (flipped, printed) = run_checklist("2 4\n\n");
        assert_eq!(
            flipped.unwrap(),
            ["--github", "--no-aws", "--no-ssh", "--docker"]
        );
        assert!(
            printed.contains("[ ] 2) AWS credentials") && printed.contains("[x] 4) Docker socket"),
            "the list is shown again after a flip:\n{printed}"
        );
        let (commas, _) = run_checklist("1,3\n\n");
        assert_eq!(
            commas.unwrap(),
            ["--no-github", "--aws", "--ssh", "--no-docker"]
        );
        let (all, _) = run_checklist("all\n\n");
        assert_eq!(all.unwrap(), ["--github", "--aws", "--ssh", "--docker"]);
        let (none, _) = run_checklist("NONE\n\n");
        assert_eq!(
            none.unwrap(),
            ["--no-github", "--no-aws", "--no-ssh", "--no-docker"]
        );
        let (none_then_one, _) = run_checklist("none\n3\n");
        assert_eq!(
            none_then_one.unwrap(),
            ["--no-github", "--no-aws", "--ssh", "--no-docker"]
        );
    }

    #[test]
    fn checklist_reprompts_on_invalid_input_and_gives_up_after_a_bound() {
        let (recovered, printed) = run_checklist("9\nyes\n2\n\n");
        assert_eq!(
            recovered.unwrap(),
            ["--github", "--no-aws", "--no-ssh", "--no-docker"],
            "invalid lines change nothing; the later valid flip applies"
        );
        assert!(printed.contains("`9` is not a row number between 1 and 4"));
        assert!(printed.contains("`yes` is not a row number"));
        // `2 9` is rejected whole: row 2 must not flip.
        let (partial, _) = run_checklist("2 9\n\n");
        assert_eq!(
            partial.unwrap(),
            ["--github", "--aws", "--no-ssh", "--no-docker"]
        );

        let (gave_up, _) = run_checklist("x\nx\nx\n\n");
        assert!(
            gave_up.is_err(),
            "repeated garbage aborts rather than guessing what to mount"
        );
    }

    #[test]
    fn jail_summary_groups_credentials_capabilities_and_forced_off() {
        let choice = |stem, enable| JailToggleChoice {
            stem,
            enable,
            implied: false,
        };
        let implied_off = |stem| JailToggleChoice {
            stem,
            enable: false,
            implied: true,
        };
        assert_eq!(
            jail_summary(
                &[
                    choice("github", true),
                    choice("gpu", true),
                    choice("ssh", true),
                    choice("mise", false),
                ],
                false
            ),
            "ai-memory: re-running inside ai-jail (credentials: GitHub CLI credentials, \
             SSH keys + agent; capabilities: GPU devices; forced off: --no-mise)"
        );
        assert_eq!(
            jail_summary(&[], false),
            "ai-memory: re-running inside ai-jail with no extra mounts"
        );
        assert_eq!(
            jail_summary(&[], true),
            "ai-memory: re-running inside ai-jail (plus the project .ai-jail)"
        );
        assert_eq!(
            jail_summary(&[choice("gpu", true)], true),
            "ai-memory: re-running inside ai-jail (capabilities: GPU devices; plus the \
             project .ai-jail)"
        );
        // An explicit selection: the user's own `no-X` is named, the rows it
        // left out are summarized, never claimed as "no extra mounts" only.
        assert_eq!(
            jail_summary(
                &[
                    choice("github", true),
                    choice("mise", false),
                    implied_off("docker"),
                    implied_off("gpu"),
                ],
                false
            ),
            "ai-memory: re-running inside ai-jail (credentials: GitHub CLI credentials; \
             forced off: --no-mise; everything else in the checklist off)"
        );
        assert_eq!(
            jail_summary(&[implied_off("docker")], false),
            "ai-memory: re-running inside ai-jail (everything else in the checklist off)"
        );
    }

    #[test]
    fn yolo_decision_defaults_to_proceed() {
        assert!(yolo_decision(""), "empty line (bare Enter) proceeds");
        assert!(yolo_decision("\n"));
        assert!(yolo_decision("y"));
        assert!(yolo_decision("Y"));
        assert!(yolo_decision("yes"));
        assert!(yolo_decision("YES"));
        assert!(
            yolo_decision("whatever"),
            "an unrecognized line still proceeds (default yes)"
        );
    }

    #[test]
    fn yolo_decision_declines_only_on_explicit_no() {
        assert!(!yolo_decision("n"));
        assert!(!yolo_decision("N"));
        assert!(!yolo_decision("no"));
        assert!(!yolo_decision("NO\n"));
    }

    #[test]
    fn read_yolo_confirmation_default_enter_proceeds_without_jail_offer() {
        let mut input = Cursor::new(b"\n".to_vec());
        let mut output = Vec::new();
        let confirmation = read_yolo_confirmation(false, &mut input, &mut output).unwrap();
        assert_eq!(
            confirmation,
            YoloConfirmation {
                proceed: true,
                jail: false
            }
        );
        let printed = String::from_utf8(output).unwrap();
        assert!(printed.contains("Proceed? [Y/n]"));
        assert!(!printed.contains("ai-jail is installed"));
    }

    #[test]
    fn read_yolo_confirmation_decline_aborts_before_the_jail_offer() {
        let mut input = Cursor::new(b"n\n".to_vec());
        let mut output = Vec::new();
        let confirmation = read_yolo_confirmation(true, &mut input, &mut output).unwrap();
        assert_eq!(
            confirmation,
            YoloConfirmation {
                proceed: false,
                jail: false
            }
        );
        let printed = String::from_utf8(output).unwrap();
        assert!(
            !printed.contains("ai-jail is installed"),
            "declining --yolo must never reach the ai-jail offer"
        );
    }

    #[test]
    fn read_yolo_confirmation_offers_ai_jail_and_reads_its_answer() {
        let mut input = Cursor::new(b"\nn\n".to_vec());
        let mut output = Vec::new();
        let confirmation = read_yolo_confirmation(true, &mut input, &mut output).unwrap();
        assert_eq!(
            confirmation,
            YoloConfirmation {
                proceed: true,
                jail: false
            }
        );
        let printed = String::from_utf8(output).unwrap();
        assert!(printed.contains("ai-jail is installed. Re-run this session inside it? [Y/n]"));
    }

    fn run_args_for_after_run_test() -> RunArgs {
        RunArgs {
            workspace: Some("work".into()),
            project: Some("repo".into()),
            workstream: None,
            new_workstream: Some("initial".into()),
            executable: None,
            yolo: true,
            true_yolo: true,
            jail: None,
            no_jail: true,
            fresh: true,
            force_unlock: false,
            no_autowire: true,
            require_server: false,
            profile: Some("work".into()),
            env: vec![("MODEL_HOME".into(), "custom".into())],
            env_file: None,
            harness: Some(RunHarnessChoice::Claude),
            native_args: vec![OsString::from("--model"), OsString::from("opus")],
        }
    }

    #[test]
    fn post_run_choice_defaults_to_quit_and_lists_same_workstream_actions() {
        let mut input = Cursor::new(b"\n");
        let mut output = Vec::new();
        let action = choose_after_run(
            ManagedHarness::Claude,
            "research",
            &[RunHarnessChoice::Codex],
            &mut input,
            &mut output,
        )
        .unwrap();

        assert_eq!(action, AfterRunAction::Quit);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("workstream 'research'"));
        assert!(output.contains("1) switch to codex"));
        assert!(output.contains("r) run claude again"));
        assert!(output.contains("q) quit (default)"));
    }

    #[test]
    fn post_run_choice_selects_an_installed_harness_by_number() {
        let mut input = Cursor::new(b"1\n");
        let mut output = Vec::new();
        assert_eq!(
            choose_after_run(
                ManagedHarness::Claude,
                "research",
                &[RunHarnessChoice::Codex],
                &mut input,
                &mut output,
            )
            .unwrap(),
            AfterRunAction::Switch(RunHarnessChoice::Codex)
        );
    }

    #[test]
    fn trailing_wrapper_options_survive_the_post_run_menu() {
        let mut args = run_args_for_after_run_test();
        args.yolo = false;
        args.true_yolo = false;
        args.jail = None;
        args.no_jail = false;
        args.no_autowire = false;
        args.native_args.extend([
            OsString::from("--yolo"),
            OsString::from("--true-yolo"),
            OsString::from("--jail=github,no-mise"),
            OsString::from("--no-autowire"),
        ]);

        preserve_trailing_run_options(&mut args);
        assert!(args.yolo);
        assert!(args.true_yolo);
        assert_eq!(args.jail.as_deref(), Some("github,no-mise"));
        assert!(!args.no_jail);
        assert!(args.no_autowire);
        assert_eq!(
            args.native_args,
            [OsString::from("--model"), OsString::from("opus")]
        );
    }

    #[test]
    fn post_run_actions_reuse_the_workstream_without_replaying_native_args() {
        let mut args = run_args_for_after_run_test();
        let outcome = RunOutcome {
            exit_code: 0,
            harness: ManagedHarness::Claude,
            interactive_session: true,
            mode: LaunchMode::Session,
            workstream_name: "research".into(),
            degraded: false,
        };

        assert!(apply_after_run_action(
            &mut args,
            &outcome,
            AfterRunAction::Switch(RunHarnessChoice::Codex)
        ));
        assert_eq!(args.harness, Some(RunHarnessChoice::Codex));
        assert_eq!(args.workstream.as_deref(), Some("research"));
        assert_eq!(args.new_workstream, None);
        assert!(!args.fresh);
        assert!(args.native_args.is_empty());
        assert!(args.yolo);
        assert!(args.true_yolo);
        assert!(args.no_jail);
        assert!(args.no_autowire);
        assert_eq!(args.env, [("MODEL_HOME".into(), "custom".into())]);
    }

    #[test]
    fn rerunning_kiro_v3_keeps_its_engine_without_replaying_user_args() {
        let mut args = run_args_for_after_run_test();
        let outcome = RunOutcome {
            exit_code: 0,
            harness: ManagedHarness::KiroV3,
            interactive_session: true,
            mode: LaunchMode::Session,
            workstream_name: "research".into(),
            degraded: false,
        };

        assert!(apply_after_run_action(
            &mut args,
            &outcome,
            AfterRunAction::Rerun
        ));
        assert_eq!(args.harness, Some(RunHarnessChoice::Kiro));
        assert_eq!(args.native_args, [OsString::from("--v3")]);
        assert_eq!(args.workstream.as_deref(), Some("research"));
    }

    #[test]
    fn post_run_prompt_requires_clean_interactive_managed_session_without_custom_binary() {
        let outcome = RunOutcome {
            exit_code: 0,
            harness: ManagedHarness::Claude,
            interactive_session: true,
            mode: LaunchMode::Session,
            workstream_name: "research".into(),
            degraded: false,
        };
        assert!(should_offer_after_run(&outcome, true, false));
        assert!(!should_offer_after_run(&outcome, false, false));
        assert!(!should_offer_after_run(&outcome, true, true));

        let mut failed = outcome;
        failed.exit_code = 1;
        assert!(!should_offer_after_run(&failed, true, false));
        failed.exit_code = 0;
        failed.mode = LaunchMode::Passthrough;
        assert!(!should_offer_after_run(&failed, true, false));
    }

    #[tokio::test(start_paused = true)]
    async fn native_session_choice_interrupt_does_not_wait_for_input() {
        let interrupted = CancellationToken::new();
        let signal = interrupted.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            signal.cancel();
        });
        let chooser = tokio::spawn(std::future::pending());
        let abort = chooser.abort_handle();
        let endpoint = ServerEndpoint::from_pair(Some("http://127.0.0.1:1".into()), None);

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_native_session_choice(chooser, &endpoint, "/unused", &interrupted),
        )
        .await;
        abort.abort();

        let error = result
            .expect("Ctrl-C must not wait for a line of stdin")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("interrupted before the agent started")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn native_session_choice_preserves_an_earlier_interrupt() {
        let interrupted = CancellationToken::new();
        interrupted.cancel();
        let chooser = tokio::spawn(std::future::pending());
        let abort = chooser.abort_handle();
        let endpoint = ServerEndpoint::from_pair(Some("http://127.0.0.1:1".into()), None);

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_native_session_choice(chooser, &endpoint, "/unused", &interrupted),
        )
        .await;
        abort.abort();

        assert!(
            result
                .expect("an earlier Ctrl-C must remain observable")
                .is_err()
        );
    }

    #[tokio::test]
    async fn native_session_choice_still_renews_before_returning_selection() {
        let heartbeats = Arc::new(AtomicUsize::new(0));
        let observed = heartbeats.clone();
        let app = Router::new().route(
            "/run/heartbeat",
            post(move || {
                observed.fetch_add(1, Ordering::SeqCst);
                async { StatusCode::NO_CONTENT }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = ServerEndpoint::from_pair(
            Some(format!("http://{}", listener.local_addr().unwrap())),
            None,
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let chooser = tokio::spawn(async { Ok(Some("selected-session".into())) });
        let selected =
            wait_for_native_session_choice(chooser, &endpoint, "/run", &CancellationToken::new())
                .await
                .unwrap()
                .unwrap();

        assert_eq!(selected.as_deref(), Some("selected-session"));
        assert_eq!(heartbeats.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn native_session_choice_interrupt_cancels_a_stalled_heartbeat() {
        let interrupted = CancellationToken::new();
        let signal = interrupted.clone();
        let app = Router::new().route(
            "/run/heartbeat",
            post(move || {
                signal.cancel();
                std::future::pending::<StatusCode>()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = ServerEndpoint::from_pair(
            Some(format!("http://{}", listener.local_addr().unwrap())),
            None,
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let chooser = tokio::spawn(async { Ok(None) });
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            wait_for_native_session_choice(chooser, &endpoint, "/run", &interrupted),
        )
        .await;
        server.abort();

        let error = result
            .expect("Ctrl-C must also cancel an in-flight heartbeat")
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("interrupted before the agent started")
        );
    }

    /// `show` filters its harness menu with this, so a false positive would
    /// offer an agent that cannot start.
    #[test]
    fn executable_available_rejects_a_program_that_is_not_installed() {
        let env = EffectiveChildEnv::from_runtime(&crate::config::RuntimeEnv::default());
        assert!(!executable_available(
            OsStr::new("ai-memory-no-such-harness-binary"),
            env.executable_search(),
        ));
    }

    /// An absolute path that exists resolves without consulting `PATH`, which
    /// is the branch `--executable` relies on.
    #[test]
    fn executable_available_accepts_an_existing_absolute_path() {
        let current = std::env::current_exe().unwrap();
        let runtime = crate::config::RuntimeEnv::default();
        let env = EffectiveChildEnv::from_runtime(&runtime);
        let search = env.executable_search();
        assert!(executable_available(current.as_os_str(), search));
        assert_eq!(
            resolve_program(current.as_os_str(), search).as_deref(),
            Some(current.as_path())
        );
    }

    #[test]
    fn resolved_run_env_overlays_windows_keys_case_insensitively() {
        let mut entries = vec![("Path".to_string(), "C:\\base".to_string())];
        upsert_env_for_platform(
            &mut entries,
            "PATH".to_string(),
            "C:\\override".to_string(),
            true,
        );

        assert_eq!(
            entries,
            vec![("PATH".to_string(), "C:\\override".to_string())]
        );
    }

    #[test]
    fn effective_child_env_keeps_unix_path_keys_case_sensitive() {
        let base = [(OsString::from("PATH"), OsString::from("/base"))];
        let overrides = [("Path".to_string(), "/override".to_string())];
        let env = EffectiveChildEnv::with_overrides_for_platform(&base, &overrides, false);

        assert_eq!(
            env.get_for_platform("PATH", false),
            Some(OsStr::new("/base"))
        );
        assert_eq!(
            env.get_for_platform("Path", false),
            Some(OsStr::new("/override"))
        );
        assert_eq!(env.entries().len(), 2);
    }

    #[test]
    fn effective_child_env_overlays_windows_path_keys_case_insensitively() {
        for override_key in ["PATH", "Path", "path"] {
            let base = [
                (OsString::from("Path"), OsString::from("C:\\base")),
                (OsString::from("PATHEXT"), OsString::from(".EXE;.CMD")),
            ];
            let overrides = [
                (override_key.to_string(), "C:\\override".to_string()),
                ("pathext".to_string(), ".BAT;.CMD".to_string()),
            ];
            let env = EffectiveChildEnv::with_overrides_for_platform(&base, &overrides, true);

            assert_eq!(
                env.get_for_platform("PATH", true),
                Some(OsStr::new("C:\\override")),
                "{override_key}"
            );
            assert_eq!(
                env.get_for_platform("PATHEXT", true),
                Some(OsStr::new(".BAT;.CMD")),
                "{override_key}"
            );
            assert_eq!(
                env.entries()
                    .iter()
                    .filter(|(key, _)| environment_keys_equal(key, OsStr::new("PATH"), true))
                    .count(),
                1,
                "{override_key}"
            );
        }
    }

    /// npm-style installs drop an extension-less shell script beside the
    /// `.cmd` wrapper. On Windows only the wrapper is launchable, so probing
    /// for mere existence reported the harness as available and the launch
    /// then failed with "program not found".
    #[cfg(windows)]
    #[test]
    fn windows_resolution_skips_the_extension_less_shim() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("faux-harness"), "#!/bin/sh\n").unwrap();

        assert_eq!(
            resolve_candidate(&tmp.path().join("faux-harness"), None),
            None,
            "an extension-less script is not launchable by CreateProcess"
        );

        std::fs::write(
            tmp.path().join("faux-harness.cmd"),
            "@echo off\r\nexit /b 0\r\n",
        )
        .unwrap();
        let resolved = resolve_candidate(
            &tmp.path().join("faux-harness"),
            Some(OsStr::new(".COM;.EXE;.BAT;.CMD")),
        )
        .expect("the wrapper resolves");
        // PATHEXT is upper-case, and Windows paths are case-insensitive, so the
        // resolved name carries whichever casing the probe used.
        assert_eq!(
            resolved
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_ascii_lowercase),
            Some("faux-harness.cmd".to_string()),
            "the PATHEXT sibling is what should be launched"
        );
        assert!(resolved.is_file());

        let status = std::process::Command::new(&resolved)
            .status()
            .expect("the resolved wrapper starts");
        assert!(status.success(), "the resolved wrapper exits successfully");
    }

    /// Unix has no PATHEXT: the file itself is the answer.
    #[cfg(not(windows))]
    #[test]
    fn unix_resolution_returns_the_file_itself() {
        let current = std::env::current_exe().unwrap();
        assert_eq!(resolve_candidate(&current, None), Some(current));
    }

    fn candidates() -> Vec<NativeSessionCandidate> {
        vec![
            NativeSessionCandidate {
                native_session_id: "newest".into(),
                updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(3_600),
            },
            NativeSessionCandidate {
                native_session_id: "older".into(),
                updated_at: SystemTime::UNIX_EPOCH,
            },
        ]
    }

    #[test]
    fn degraded_session_correlation_requires_one_new_session() {
        let started = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let before = std::collections::HashSet::from(["old".to_string()]);
        let candidate = |id: &str| NativeSessionCandidate {
            native_session_id: id.into(),
            updated_at: started + Duration::from_secs(1),
        };
        assert_eq!(
            unique_new_session(vec![candidate("old"), candidate("new")], &before, started).unwrap(),
            "new"
        );
        assert!(
            unique_new_session(
                vec![candidate("first"), candidate("second")],
                &before,
                started
            )
            .unwrap_err()
            .to_string()
            .contains("found 2")
        );
        assert!(unique_new_session(vec![candidate("old")], &before, started).is_err());
    }

    #[test]
    fn relative_session_dir_is_persisted_against_launch_cwd() {
        assert_eq!(
            absolute_session_dir(Some(Path::new("stores/native")), Path::new("/repo")),
            Some(PathBuf::from("/repo/stores/native"))
        );
        assert_eq!(
            absolute_session_dir(Some(Path::new("/stores/native")), Path::new("/repo")),
            Some(PathBuf::from("/stores/native"))
        );
    }

    #[test]
    fn native_grok_rules_flags_suppress_context_injection() {
        for args in [
            vec![OsString::from("--rules"), OsString::from("be terse")],
            vec![OsString::from("--rules=be terse")],
            vec![
                OsString::from("--append-system-prompt"),
                OsString::from("x"),
            ],
        ] {
            assert!(user_supplied_grok_rules(&args), "{args:?}");
        }
        assert!(!user_supplied_grok_rules(&[
            OsString::from("--model"),
            OsString::from("grok-4.5")
        ]));
    }

    #[test]
    fn lease_owner_uses_the_resolved_host_and_process() {
        assert_eq!(lease_owner_label(Some("workstation"), 42), "workstation:42");
        assert_eq!(lease_owner_label(None, 42), "localhost:42");
    }

    #[test]
    fn heartbeat_health_reports_each_outage_and_recovery_once() {
        let mut health = HeartbeatHealth::default();

        assert!(health.record_failure());
        assert!(!health.record_failure());
        assert!(!health.record_failure());
        assert!(health.record_success());
        assert!(!health.record_success());
        assert!(health.record_failure());
    }

    #[tokio::test]
    async fn managed_heartbeat_times_out_and_recovers() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let handler_attempts = Arc::clone(&attempts);
        let app = Router::new().route(
            "/workstream/runs/{run_id}/heartbeat",
            post(move || {
                let attempts = Arc::clone(&handler_attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let endpoint = ServerEndpoint::from_pair(Some(format!("http://{address}")), None);
        let mut health = HeartbeatHealth::default();

        assert!(
            send_managed_heartbeat_with_timeout(
                &endpoint,
                "/workstream/runs/test",
                &mut health,
                Duration::from_millis(10),
            )
            .await
            .is_err()
        );
        assert_eq!(health.consecutive_failures, 1);
        send_managed_heartbeat_with_timeout(
            &endpoint,
            "/workstream/runs/test",
            &mut health,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(health.consecutive_failures, 0);

        server.abort();
    }

    #[tokio::test]
    async fn prepare_waits_for_a_previous_launcher_to_finish() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let handler_attempts = Arc::clone(&attempts);
        let app = Router::new().route(
            "/workstream/runs",
            post(move || {
                let attempts = Arc::clone(&handler_attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                        return (
                            StatusCode::CONFLICT,
                            axum::Json(serde_json::json!({
                                "error": "workstream is already active: owned by workstation:42"
                            })),
                        )
                            .into_response();
                    }
                    axum::Json(PrepareManagedRunResponse {
                        workstream_id: WorkstreamId::new(),
                        workstream_name: "default".into(),
                        run_id: ManagedRunId::new(),
                        resolved_agent: Some(AgentKind::Codex),
                        native_session_id: None,
                        source_cursor: None,
                        sync_after: 0,
                        sync_through: 0,
                        may_adopt_existing_session: false,
                        manifest_warning: None,
                    })
                    .into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let endpoint = ServerEndpoint::from_pair(Some(format!("http://{address}")), None);
        let request = PrepareManagedRunRequest {
            workspace: "default".into(),
            project: "project".into(),
            cwd: "/tmp/project".into(),
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            agent: AgentKind::Codex,
            automatic_harness: false,
            available_agents: Vec::new(),
            workstream: None,
            new_workstream: None,
            force_unlock: false,
            lease_owner: "workstation:43".into(),
            repository_identity: None,
        };

        let prepared = prepare_managed_run_with_retry(
            &endpoint,
            &request,
            // A wall-clock give-up deadline, not a latency assertion. This
            // test is about the retry loop reaching the third attempt, and at
            // 100ms it was really asserting that three HTTP round-trips fit
            // inside 100ms — which a loaded CI runner does not guarantee, so
            // it failed intermittently on both ubuntu and macOS with the
            // 409 the mock is supposed to retry past. The window is generous
            // because nothing here should depend on its size; the loop still
            // returns the instant the third attempt succeeds, so the fast
            // path stays a few milliseconds.
            Duration::from_secs(5),
            Duration::from_millis(1),
            true,
        )
        .await
        .result
        .unwrap();

        assert_eq!(prepared.workstream_name, "default");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn busy_retry_keeps_first_warning_and_final_error_unchanged() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let handler_attempts = Arc::clone(&attempts);
        let app = Router::new().route(
            "/workstream/runs",
            post(move || {
                let attempt = handler_attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    let until = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(60);
                    match attempt {
                        0 => (
                            StatusCode::CONFLICT,
                            axum::Json(serde_json::json!({
                                "error": format!(
                                    "workstream is already active: owned by first until {until}"
                                ),
                                "manifest_warning": "first repair warning"
                            })),
                        )
                            .into_response(),
                        1 => (
                            StatusCode::CONFLICT,
                            axum::Json(serde_json::json!({
                                "error": format!(
                                    "workstream is already active: owned by second until {until}"
                                )
                            })),
                        )
                            .into_response(),
                        _ => (
                            StatusCode::NOT_FOUND,
                            axum::Json(serde_json::json!({
                                "error": "final missing workstream",
                                "manifest_warning": "later repair warning"
                            })),
                        )
                            .into_response(),
                    }
                }
            }),
        );
        let (endpoint, server) = serve(app).await;
        let outcome = prepare_managed_run_with_retry(
            &endpoint,
            &held_lease_request(),
            Duration::from_secs(5),
            Duration::from_millis(1),
            false,
        )
        .await;
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(
            outcome.manifest_warning.as_deref(),
            Some("first repair warning"),
            "the first warning wins over a later distinct warning"
        );

        let mut output = Vec::new();
        let error = complete_prepare_run(outcome, &mut output).expect_err("final 404 is preserved");
        let response = error.downcast_ref::<ServerResponseError>().unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        assert!(response.body().contains("final missing workstream"));
        assert!(response.body().contains("later repair warning"));
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "ai-memory: warning: first repair warning\n"
        );
        server.abort();
    }

    #[tokio::test]
    async fn busy_retry_returns_first_warning_with_later_success() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let handler_attempts = Arc::clone(&attempts);
        let app = Router::new().route(
            "/workstream/runs",
            post(move || {
                let attempt = handler_attempts.fetch_add(1, Ordering::SeqCst);
                async move {
                    if attempt == 0 {
                        let until = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(60);
                        return (
                            StatusCode::CONFLICT,
                            axum::Json(serde_json::json!({
                                "error": format!(
                                    "workstream is already active: owned by first until {until}"
                                ),
                                "manifest_warning": "first repair warning"
                            })),
                        )
                            .into_response();
                    }
                    axum::Json(PrepareManagedRunResponse {
                        workstream_id: WorkstreamId::new(),
                        workstream_name: "default".into(),
                        run_id: ManagedRunId::new(),
                        resolved_agent: Some(AgentKind::Codex),
                        native_session_id: None,
                        source_cursor: None,
                        sync_after: 0,
                        sync_through: 0,
                        may_adopt_existing_session: false,
                        manifest_warning: None,
                    })
                    .into_response()
                }
            }),
        );
        let (endpoint, server) = serve(app).await;
        let outcome = prepare_managed_run_with_retry(
            &endpoint,
            &held_lease_request(),
            Duration::from_secs(5),
            Duration::from_millis(1),
            false,
        )
        .await;
        let mut output = Vec::new();
        let response = complete_prepare_run(outcome, &mut output).unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(
            response.manifest_warning.as_deref(),
            Some("first repair warning")
        );
        assert!(
            output.is_empty(),
            "success returns the warning for one caller print"
        );
        server.abort();
    }

    #[test]
    fn managed_run_error_extracts_manifest_warning() {
        let error = crate::http_client::server_response_error_for_test(
            reqwest::Method::POST,
            "/workstream/runs",
            reqwest::StatusCode::NOT_FOUND,
            serde_json::json!({
                "error": "managed workstream 'missing' not found",
                "manifest_warning": "project name promotion committed; repair needed"
            })
            .to_string(),
        );
        assert_eq!(
            server_manifest_warning(&error).as_deref(),
            Some("project name promotion committed; repair needed")
        );
    }

    #[test]
    fn held_lease_parses_the_store_busy_message() {
        let held = parse_held_lease(
            "workstream is already active: owned by ai-sandbox:2 until 2026-10-01T04:28:44.647329Z",
        )
        .expect("current store format parses");
        assert_eq!(held.owner, "ai-sandbox:2");
        assert_eq!(
            held.expires,
            "2026-10-01T04:28:44.647329Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
        );
        // An older server's message carries no expiry: nothing to wait on.
        assert_eq!(
            parse_held_lease("workstream is already active: owned by workstation:42"),
            None
        );
        assert_eq!(parse_held_lease("some other conflict"), None);
    }

    #[test]
    fn held_lease_wait_is_bounded_by_one_lease() {
        let now = "2026-10-01T04:00:00Z".parse::<jiff::Timestamp>().unwrap();
        let at = |secs: i64| now + jiff::SignedDuration::from_secs(secs);
        let slack = Duration::from_secs(1);
        assert_eq!(
            held_lease_wait(at(30), now, slack),
            Some(Duration::from_secs(31))
        );
        // Already lapsed: retry right after the slack.
        assert_eq!(held_lease_wait(at(-5), now, slack), Some(slack));
        // Further out than one lease means a renewing (live) owner.
        assert_eq!(held_lease_wait(at(600), now, slack), None);
    }

    fn held_lease_server(
        conflicts: usize,
        lease: Duration,
    ) -> (Router, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let handler_attempts = Arc::clone(&attempts);
        let app = Router::new().route(
            "/workstream/runs",
            post(move || {
                let attempts = Arc::clone(&handler_attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) < conflicts {
                        let until = jiff::Timestamp::now()
                            + jiff::SignedDuration::try_from(lease).unwrap();
                        return (
                            StatusCode::CONFLICT,
                            axum::Json(serde_json::json!({
                                "error": format!(
                                    "workstream is already active: owned by ai-sandbox:2 until {until}"
                                )
                            })),
                        )
                            .into_response();
                    }
                    axum::Json(PrepareManagedRunResponse {
                        workstream_id: WorkstreamId::new(),
                        workstream_name: "default".into(),
                        run_id: ManagedRunId::new(),
                        resolved_agent: Some(AgentKind::ClaudeCode),
                        native_session_id: None,
                        source_cursor: None,
                        sync_after: 0,
                        sync_through: 0,
                        may_adopt_existing_session: false,
                        manifest_warning: None,
                    })
                    .into_response()
                }
            }),
        );
        (app, attempts)
    }

    fn held_lease_request() -> PrepareManagedRunRequest {
        PrepareManagedRunRequest {
            workspace: "default".into(),
            project: "project".into(),
            cwd: "/tmp/project".into(),
            repo_fingerprint: "repo".into(),
            worktree_fingerprint: "worktree".into(),
            agent: AgentKind::ClaudeCode,
            automatic_harness: false,
            available_agents: Vec::new(),
            workstream: None,
            new_workstream: None,
            force_unlock: false,
            lease_owner: "workstation:43".into(),
            repository_identity: None,
        }
    }

    async fn serve(app: Router) -> (ServerEndpoint, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (
            ServerEndpoint::from_pair(Some(format!("http://{address}")), None),
            server,
        )
    }

    /// The Ctrl-C-then-relaunch case: a lease left behind by a launcher that
    /// could not release it is waited out, then the launch proceeds by itself.
    #[tokio::test]
    async fn interactive_launch_waits_out_a_lapsing_lease_then_proceeds() {
        let (app, attempts) = held_lease_server(1, Duration::from_millis(300));
        let (endpoint, server) = serve(app).await;
        let request = held_lease_request();
        let first =
            post_json::<_, PrepareManagedRunResponse>(&endpoint, "/workstream/runs", &request)
                .await
                .expect_err("the first attempt sees the held lease");
        let started = std::time::Instant::now();
        let prepared = wait_out_held_lease(
            &endpoint,
            &request,
            PrepareRunOutcome::from_result(Err(first)),
            &CancellationToken::new(),
            Duration::ZERO,
            Duration::from_millis(50),
        )
        .await
        .result
        .expect("proceeds once the lease lapsed");
        assert_eq!(prepared.workstream_name, "default");
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "it waited for the expiry"
        );
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn wait_out_keeps_the_initial_warning_through_a_successful_retry() {
        let (app, attempts) = held_lease_server(0, Duration::ZERO);
        let (endpoint, server) = serve(app).await;
        let request = held_lease_request();
        let until = jiff::Timestamp::now() + jiff::SignedDuration::from_millis(10);
        let first = crate::http_client::server_response_error_for_test(
            reqwest::Method::POST,
            "/workstream/runs",
            reqwest::StatusCode::CONFLICT,
            serde_json::json!({
                "error": format!("workstream is already active: owned by first until {until}"),
                "manifest_warning": "first repair warning"
            })
            .to_string(),
        );
        let outcome = wait_out_held_lease(
            &endpoint,
            &request,
            PrepareRunOutcome::from_result(Err(first)),
            &CancellationToken::new(),
            Duration::ZERO,
            Duration::from_secs(1),
        )
        .await;
        let mut output = Vec::new();
        let response = complete_prepare_run(outcome, &mut output).unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(
            response.manifest_warning.as_deref(),
            Some("first repair warning")
        );
        assert!(output.is_empty());
        server.abort();
    }

    /// Adversarial: a live owner keeps renewing. The waiter must report it, not
    /// loop forever and never take the lease.
    #[tokio::test]
    async fn a_renewed_lease_is_reported_as_a_live_owner_not_taken_over() {
        let (app, _) = held_lease_server(usize::MAX, Duration::from_millis(150));
        let (endpoint, server) = serve(app).await;
        let request = held_lease_request();
        let first =
            post_json::<_, PrepareManagedRunResponse>(&endpoint, "/workstream/runs", &request)
                .await
                .expect_err("held");
        let error = wait_out_held_lease(
            &endpoint,
            &request,
            PrepareRunOutcome::from_result(Err(first)),
            &CancellationToken::new(),
            Duration::ZERO,
            Duration::from_millis(50),
        )
        .await
        .result
        .expect_err("a renewing owner is never displaced");
        assert!(
            format!("{error:#}").contains("renewed the lease"),
            "{error:#}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn force_unlock_asks_once_instead_of_waiting_out_the_lease() {
        let (app, attempts) = held_lease_server(usize::MAX, Duration::from_secs(60));
        let (endpoint, server) = serve(app).await;
        let mut request = held_lease_request();
        request.force_unlock = true;
        let started = std::time::Instant::now();

        let error = prepare_managed_run(&endpoint, &request, true, &CancellationToken::new())
            .await
            .expect_err("a server that refuses the takeover must fail immediately");

        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(
            format!("{error:#}").contains("--force-unlock was refused"),
            "{error:#}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn ctrl_c_aborts_the_held_lease_wait_immediately() {
        let (app, attempts) = held_lease_server(1, Duration::from_secs(60));
        let (endpoint, server) = serve(app).await;
        let request = held_lease_request();
        let first =
            post_json::<_, PrepareManagedRunResponse>(&endpoint, "/workstream/runs", &request)
                .await
                .expect_err("held");
        let interrupted = CancellationToken::new();
        interrupted.cancel();
        let started = std::time::Instant::now();
        let error = wait_out_held_lease(
            &endpoint,
            &request,
            PrepareRunOutcome::from_result(Err(first)),
            &interrupted,
            Duration::ZERO,
            Duration::from_millis(50),
        )
        .await
        .result
        .expect_err("interrupted");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no retry after Ctrl-C"
        );
        server.abort();
    }

    fn auto_candidate(harness: ManagedHarness, updated: u64) -> AutoSessionCandidate {
        AutoSessionCandidate {
            harness,
            session: NativeSessionCandidate {
                native_session_id: format!("{}-{updated}", harness.as_str()),
                updated_at: SystemTime::UNIX_EPOCH + Duration::from_secs(updated),
            },
        }
    }

    #[cfg(unix)]
    #[test]
    fn automatic_opencode_probe_failure_keeps_another_harness_available() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for (name, body) in [
            ("opencode", "#!/bin/sh\nprintf 'OpenCode latest\\n'\n"),
            ("codex", "#!/bin/sh\nexit 0\n"),
        ] {
            let path = bin.join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env = EffectiveChildEnv::for_tests([("PATH", bin.as_os_str())]);
        let search = env.executable_search();

        assert!(
            resolve_opencode_for_launch(None, None, &env)
                .unwrap()
                .is_none()
        );
        let usable = filter_usable_auto_sessions(
            vec![auto_candidate(ManagedHarness::Codex, 100)],
            |harness| executable_available(OsStr::new(harness.executable()), search),
        )
        .unwrap();
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].harness, ManagedHarness::Codex);

        let explicit =
            resolve_opencode_for_launch(Some(RunHarnessChoice::OpenCode), None, &env).unwrap_err();
        assert!(
            format!("{explicit:#}").contains("could not determine the OpenCode major"),
            "{explicit:#}"
        );
    }

    #[test]
    fn automatic_selection_skips_newer_sessions_for_unavailable_harnesses() {
        let candidates = vec![
            auto_candidate(ManagedHarness::Claude, 200),
            auto_candidate(ManagedHarness::Codex, 100),
        ];
        let usable =
            filter_usable_auto_sessions(candidates, |harness| harness == ManagedHarness::Codex)
                .unwrap();
        assert_eq!(usable.len(), 1);
        assert_eq!(usable[0].harness, ManagedHarness::Codex);

        let error =
            filter_usable_auto_sessions(vec![auto_candidate(ManagedHarness::Claude, 200)], |_| {
                false
            })
            .unwrap_err();
        assert!(error.to_string().contains("claude"));
    }

    #[test]
    fn adoption_prompt_defaults_to_newest_checkout_session() {
        let mut input = Cursor::new(b"\n");
        let mut output = Vec::new();
        let selected = choose_native_session(
            ManagedHarness::Codex,
            "default",
            &candidates(),
            &mut input,
            &mut output,
            SystemTime::UNIX_EPOCH + Duration::from_secs(7_200),
        )
        .unwrap();
        assert_eq!(selected.as_deref(), Some("newest"));
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("no codex session is linked"));
        assert!(rendered.contains("updated 1 hour ago"));
        assert!(rendered.contains("Start a new codex session"));
    }

    #[test]
    fn adoption_prompt_can_start_fresh_or_select_an_older_session() {
        let mut fresh_input = Cursor::new(b"0\n");
        let mut output = Vec::new();
        assert!(
            choose_native_session(
                ManagedHarness::Claude,
                "default",
                &candidates(),
                &mut fresh_input,
                &mut output,
                SystemTime::UNIX_EPOCH,
            )
            .unwrap()
            .is_none()
        );

        let mut older_input = Cursor::new(b"invalid\n2\n");
        let selected = choose_native_session(
            ManagedHarness::Codex,
            "default",
            &candidates(),
            &mut older_input,
            &mut output,
            SystemTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(selected.as_deref(), Some("older"));
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("Enter 0 through 2.")
        );
    }

    #[test]
    fn native_arguments_do_not_require_separator_and_wrapper_yolo_is_consumed() {
        let cli = Cli::try_parse_from([
            OsStr::new("ai-memory"),
            OsStr::new("run"),
            OsStr::new("--project"),
            OsStr::new("memory"),
            OsStr::new("codex"),
            OsStr::new("--yolo"),
            OsStr::new("-m"),
            OsStr::new("gpt-5"),
            OsStr::new("continue here"),
        ])
        .unwrap();
        let CliCommand::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.project.as_deref(), Some("memory"));
        assert!(args.yolo);
        assert_eq!(
            args.native_args,
            ["-m", "gpt-5", "continue here"]
                .map(OsString::from)
                .to_vec()
        );
    }

    #[test]
    fn opencode_name_and_native_flags_parse_without_separator() {
        let cli = Cli::try_parse_from([
            OsStr::new("ai-memory"),
            OsStr::new("run"),
            OsStr::new("opencode"),
            OsStr::new("run"),
            OsStr::new("--model"),
            OsStr::new("provider/model"),
            OsStr::new("continue here"),
        ])
        .unwrap();
        let CliCommand::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert!(matches!(
            args.harness,
            Some(crate::cli::RunHarnessChoice::OpenCode)
        ));
        assert_eq!(
            args.native_args,
            ["run", "--model", "provider/model", "continue here"]
                .map(OsString::from)
                .to_vec()
        );
    }

    #[test]
    fn kimi_cli_alias_selects_the_kimi_adapter() {
        let cli = Cli::try_parse_from([
            OsStr::new("ai-memory"),
            OsStr::new("run"),
            OsStr::new("kimi-cli"),
            OsStr::new("--model"),
            OsStr::new("kimi-for-coding"),
        ])
        .unwrap();
        let CliCommand::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert!(matches!(
            args.harness,
            Some(crate::cli::RunHarnessChoice::Kimi)
        ));
        assert_eq!(
            args.native_args,
            ["--model", "kimi-for-coding"].map(OsString::from).to_vec()
        );
    }

    #[test]
    fn copilot_aliases_select_the_managed_adapter() {
        for name in ["copilot", "copilot-cli"] {
            let cli = Cli::try_parse_from([
                OsStr::new("ai-memory"),
                OsStr::new("run"),
                OsStr::new(name),
                OsStr::new("--model"),
                OsStr::new("gpt-5.6"),
            ])
            .unwrap();
            let CliCommand::Run(args) = cli.command else {
                panic!("expected run command");
            };
            let choice = args.harness.expect("explicit harness");
            assert_eq!(choice, crate::cli::RunHarnessChoice::Copilot, "{name}");
            assert_eq!(managed_harness(choice), ManagedHarness::Copilot);
            assert_eq!(run_harness_choice(ManagedHarness::Copilot), choice);
            assert_eq!(
                args.native_args,
                ["--model", "gpt-5.6"].map(OsString::from).to_vec()
            );
        }
        assert_eq!(
            managed_harness_from_agent(AgentKind::CopilotCli),
            Some(ManagedHarness::Copilot)
        );
    }

    #[test]
    fn command_code_aliases_select_the_managed_adapter() {
        for name in ["command-code", "commandcode", "cmdc", "cmd"] {
            let cli = Cli::try_parse_from([
                OsStr::new("ai-memory"),
                OsStr::new("run"),
                OsStr::new(name),
                OsStr::new("--print"),
                OsStr::new("continue here"),
            ])
            .unwrap();
            let CliCommand::Run(args) = cli.command else {
                panic!("expected run command");
            };
            assert!(
                matches!(
                    args.harness,
                    Some(crate::cli::RunHarnessChoice::CommandCode)
                ),
                "{name}"
            );
            assert_eq!(
                args.native_args,
                ["--print", "continue here"].map(OsString::from).to_vec()
            );
        }
    }

    #[test]
    fn kiro_cli_alias_selects_the_kiro_adapter() {
        for name in ["kiro", "kiro-cli"] {
            let cli = Cli::try_parse_from([
                OsStr::new("ai-memory"),
                OsStr::new("run"),
                OsStr::new(name),
                OsStr::new("--model"),
                OsStr::new("sonnet"),
            ])
            .unwrap();
            let CliCommand::Run(args) = cli.command else {
                panic!("expected run command");
            };
            assert!(
                matches!(args.harness, Some(crate::cli::RunHarnessChoice::Kiro)),
                "{name}"
            );
            assert_eq!(
                args.native_args,
                ["--model", "sonnet"].map(OsString::from).to_vec()
            );
            assert_eq!(managed_harness(args.harness.unwrap()), ManagedHarness::Kiro);
        }
    }

    #[test]
    fn kiro_engine_resolution_prefers_explicit_args_then_persisted_flavor() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let v3_cursor = serde_json::json!({
            "path": "/sanitized/messages.jsonl",
            "offset": 42,
            "flavor": "kiro-v3",
            "prefix_sha256": "fixture"
        })
        .to_string();

        assert_eq!(
            managed_harness_for_args(RunHarnessChoice::Kiro, &[OsString::from("--v3")]),
            ManagedHarness::KiroV3
        );
        assert_eq!(
            resolve_kiro_harness(
                &[],
                Some("missing-session"),
                Some(&v3_cursor),
                ManagedHarness::Kiro,
                temp.path(),
                &cwd,
            )
            .unwrap(),
            ManagedHarness::KiroV3
        );
        assert_eq!(
            resolve_kiro_harness(
                &[OsString::from("--agent-engine=v2")],
                Some("sess_c3774f9d-269e-40d1-aa02-2bb0c0817b4e"),
                Some(&v3_cursor),
                ManagedHarness::KiroV3,
                temp.path(),
                &cwd,
            )
            .unwrap(),
            ManagedHarness::Kiro
        );
        assert_eq!(
            resolve_kiro_harness(
                &[
                    OsString::from("--resume-id"),
                    OsString::from("missing-explicit-session"),
                ],
                Some("sess_c3774f9d-269e-40d1-aa02-2bb0c0817b4e"),
                Some(&v3_cursor),
                ManagedHarness::KiroV3,
                temp.path(),
                &cwd,
            )
            .unwrap(),
            ManagedHarness::Kiro
        );
    }

    #[test]
    fn automatic_kiro_flavors_share_one_server_agent_identity() {
        let candidates = vec![
            AutoSessionCandidate {
                harness: ManagedHarness::KiroV3,
                session: NativeSessionCandidate {
                    native_session_id: "v3".into(),
                    updated_at: SystemTime::UNIX_EPOCH,
                },
            },
            AutoSessionCandidate {
                harness: ManagedHarness::Kiro,
                session: NativeSessionCandidate {
                    native_session_id: "v2".into(),
                    updated_at: SystemTime::UNIX_EPOCH,
                },
            },
        ];

        assert_eq!(unique_auto_agents(&candidates), [AgentKind::KiroCli]);
        assert_eq!(
            automatic_harness_flavor(ManagedHarness::Kiro, ManagedHarness::KiroV3, None, None,),
            ManagedHarness::KiroV3
        );
        assert_eq!(
            automatic_harness_flavor(
                ManagedHarness::Kiro,
                ManagedHarness::KiroV3,
                Some("linked"),
                None,
            ),
            ManagedHarness::Kiro
        );
    }

    #[test]
    fn automatic_opencode_selection_keeps_the_resolved_major() {
        assert_eq!(
            automatic_harness_flavor(
                ManagedHarness::OpenCode,
                ManagedHarness::OpenCode2,
                Some("linked"),
                Some(ManagedHarness::OpenCode2),
            ),
            ManagedHarness::OpenCode2
        );
    }

    #[test]
    fn incompatible_link_starts_fresh_in_the_explicit_kiro_engine() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let env = EffectiveChildEnv::default();
        let (fresh, orphaned) = build_preflighted_launch_plan(
            ManagedHarness::KiroV3,
            None,
            vec![OsString::from("--v3")],
            Some("3f6d1c2a-0000-4000-8000-000000000aaa"),
            false,
            temp.path(),
            &cwd,
            &env,
        )
        .unwrap();
        assert_eq!(
            orphaned.as_deref(),
            Some("3f6d1c2a-0000-4000-8000-000000000aaa")
        );
        assert_eq!(fresh.expected_session_id, None);
        assert_eq!(fresh.args, [OsString::from("--v3")]);
    }

    #[test]
    fn bare_run_and_wrapper_yolo_parse_without_a_harness() {
        let cli = Cli::try_parse_from(["ai-memory", "run", "--yolo"]).unwrap();
        let CliCommand::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert!(args.harness.is_none());
        assert!(args.yolo);
        assert!(args.native_args.is_empty());
    }

    #[test]
    fn trailing_wrapper_yolo_is_removed_before_native_resume_detection() {
        let mut args = ["--yolo", "resume", "native-id"]
            .map(OsString::from)
            .to_vec();
        assert!(remove_wrapper_yolo(&mut args));
        assert_eq!(args, ["resume", "native-id"].map(OsString::from));
    }

    /// Right after the harness, clap parses the wrapper flags itself; once a
    /// native argument starts, trailing_var_arg swallows everything after it
    /// into `native_args`. `--yolo` was stripped from there, but a swallowed
    /// `--true-yolo` was forwarded to Claude as an unknown option and the
    /// bypass never applied. Both positions must yield the wrapper flag.
    #[test]
    fn true_yolo_is_a_wrapper_flag_in_either_position() {
        let parse = |argv: &[&str]| {
            let CliCommand::Run(args) = Cli::try_parse_from(argv).unwrap().command else {
                panic!("expected run command");
            };
            args
        };

        let direct = parse(&[
            "ai-memory",
            "run",
            "claude",
            "--yolo",
            "--true-yolo",
            "--model",
            "opus",
        ]);
        assert!(direct.yolo && direct.true_yolo);
        assert_eq!(direct.native_args, ["--model", "opus"].map(OsString::from));

        let swallowed = parse(&[
            "ai-memory",
            "run",
            "claude",
            "--model",
            "opus",
            "--true-yolo",
        ]);
        assert!(
            !swallowed.true_yolo,
            "clap leaves it in the native argv here"
        );
        let mut native = swallowed.native_args;
        assert!(remove_wrapper_true_yolo(&mut native));
        assert_eq!(native, ["--model", "opus"].map(OsString::from));
    }

    #[test]
    fn true_yolo_flag_implies_yolo_on_every_harness() {
        assert_eq!(
            yolo_modes(false, true, false),
            YoloModes {
                yolo: true,
                claude_true_yolo: true
            },
            "--true-yolo alone must still warn/offer ai-jail and map the harness's yolo"
        );
        // Both together are redundant, never an error or a different result.
        assert_eq!(
            yolo_modes(true, true, false),
            yolo_modes(false, true, false)
        );
    }

    #[test]
    fn plain_yolo_does_not_bypass_claude_permissions_by_itself() {
        assert_eq!(
            yolo_modes(true, false, false),
            YoloModes {
                yolo: true,
                claude_true_yolo: false
            }
        );
        assert_eq!(
            yolo_modes(false, false, false),
            YoloModes {
                yolo: false,
                claude_true_yolo: false
            }
        );
    }

    /// The config key upgrades a yolo launch, but alone must never turn an
    /// ordinary managed run into a `bypassPermissions` one with no `--yolo`
    /// warning (it previously did).
    #[test]
    fn claude_true_yolo_config_only_upgrades_a_yolo_launch() {
        assert_eq!(
            yolo_modes(false, false, true),
            YoloModes {
                yolo: false,
                claude_true_yolo: false
            }
        );
        assert_eq!(
            yolo_modes(true, false, true),
            YoloModes {
                yolo: true,
                claude_true_yolo: true
            }
        );
    }

    #[test]
    fn wrapper_fresh_parses_before_or_after_the_harness() {
        let cli = Cli::try_parse_from(["ai-memory", "run", "--fresh", "codex"]).unwrap();
        let CliCommand::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert!(args.fresh);
        assert!(args.native_args.is_empty());

        let mut trailing = ["--fresh", "--model", "opus"].map(OsString::from).to_vec();
        assert!(remove_wrapper_fresh(&mut trailing));
        assert_eq!(trailing, ["--model", "opus"].map(OsString::from));
    }

    #[test]
    fn force_unlock_is_a_wrapper_flag_before_or_after_the_harness() {
        let cli = Cli::try_parse_from([
            "ai-memory",
            "run",
            "--force-unlock",
            "codex",
            "--model",
            "gpt-5",
        ])
        .unwrap();
        let CliCommand::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert!(args.force_unlock);
        assert_eq!(args.native_args, ["--model", "gpt-5"].map(OsString::from));

        let mut trailing = ["--model", "gpt-5", "--force-unlock"]
            .map(OsString::from)
            .to_vec();
        assert!(remove_wrapper_force_unlock(&mut trailing));
        assert_eq!(trailing, ["--model", "gpt-5"].map(OsString::from));
    }

    #[test]
    fn wrapper_no_autowire_parses_before_or_after_the_harness() {
        // Before the harness: clap binds it as the wrapper flag.
        let cli = Cli::try_parse_from(["ai-memory", "run", "--no-autowire", "kimi"]).unwrap();
        let CliCommand::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert!(args.no_autowire);

        // After the harness: trailing_var_arg swallows it into native_args, so it
        // must be extracted rather than forwarded to the harness (which would
        // reject an unknown flag).
        let mut trailing = ["--no-autowire", "--model", "opus"]
            .map(OsString::from)
            .to_vec();
        assert!(remove_wrapper_no_autowire(&mut trailing));
        assert_eq!(trailing, ["--model", "opus"].map(OsString::from));
    }

    #[test]
    fn resolve_run_env_merges_profile_file_then_cli_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vars.env");
        std::fs::write(&path, "# a comment\n\n  \nFOO=from-file\nBAR=keep\n").unwrap();

        let profile = crate::config::RunProfile {
            env: [
                ("FOO".to_string(), "from-profile".to_string()),
                ("PROFILE_ONLY".to_string(), "present".to_string()),
            ]
            .into(),
        };
        let cli_pairs = vec![("FOO".to_string(), "from-cli".to_string())];
        let merged = resolve_run_env(Some(&profile), Some(&path), &cli_pairs).unwrap();

        assert_eq!(
            merged,
            vec![
                ("FOO".to_string(), "from-cli".to_string()),
                ("PROFILE_ONLY".to_string(), "present".to_string()),
                ("BAR".to_string(), "keep".to_string()),
            ]
        );
    }

    #[test]
    fn wrapper_profile_parses_before_harness_without_stealing_native_profile() {
        let wrapper = parse_run(&[
            "ai-memory",
            "run",
            "--profile",
            "work",
            "claude",
            "--model",
            "opus",
        ]);
        assert_eq!(wrapper.profile.as_deref(), Some("work"));
        assert_eq!(wrapper.native_args, ["--model", "opus"].map(OsString::from));

        for (harness, native_profile) in [("omp", "omp-work"), ("codex", "codex-work")] {
            let native = parse_run(&["ai-memory", "run", harness, "--profile", native_profile]);
            assert_eq!(native.profile, None, "{harness}");
            assert_eq!(
                native.native_args,
                ["--profile", native_profile].map(OsString::from),
                "{harness}'s own --profile must reach the child"
            );
        }
    }

    #[tokio::test]
    async fn unknown_run_profile_fails_before_server_or_child_work() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config::load(None, Some(tmp.path().join("data"))).unwrap();
        config.server_url = "http://127.0.0.1:1".into();
        let args = RunArgs {
            workspace: None,
            project: None,
            workstream: None,
            new_workstream: None,
            executable: Some(PathBuf::from("definitely-not-a-harness")),
            yolo: false,
            true_yolo: false,
            jail: None,
            no_jail: true,
            fresh: false,
            force_unlock: false,
            no_autowire: true,
            require_server: false,
            profile: Some("missing".into()),
            env: Vec::new(),
            env_file: None,
            harness: Some(RunHarnessChoice::Claude),
            native_args: Vec::new(),
        };

        let error = run_from_with_wiring(
            &config,
            args,
            tmp.path(),
            &super::super::run_autowire::WireOverrides::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown run profile \"missing\"; none are defined. Add to your config.toml:\n\n  \
             [run.profiles.missing.env]\n  CLAUDE_CONFIG_DIR = \"/absolute/path\""
        );
    }

    #[test]
    fn unknown_run_profile_names_the_defined_profiles_in_sorted_order() {
        let mut run = crate::config::RunSettings::default();
        for name in ["work", "personal"] {
            run.profiles
                .insert(name.into(), crate::config::RunProfile::default());
        }
        assert_eq!(
            unknown_run_profile(&run, "wrok").to_string(),
            "unknown run profile \"wrok\"; defined profiles: personal, work"
        );
    }

    #[test]
    fn unknown_run_profile_hint_quotes_a_dotted_name_as_one_toml_key() {
        let hint =
            unknown_run_profile(&crate::config::RunSettings::default(), "work.v2").to_string();
        assert!(
            hint.contains("[run.profiles.\"work.v2\".env]"),
            "a bare dotted key would paste as nested tables: {hint}"
        );
    }

    #[test]
    fn resolve_run_env_without_a_file_returns_only_cli_pairs() {
        let cli_pairs = vec![("A".to_string(), "1".to_string())];
        let merged = resolve_run_env(None, None, &cli_pairs).unwrap();
        assert_eq!(merged, vec![("A".to_string(), "1".to_string())]);
    }

    #[test]
    fn resolve_run_env_rejects_a_malformed_env_file_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.env");
        std::fs::write(&path, "NOVALUE\n").unwrap();
        let error = resolve_run_env(None, Some(&path), &[]).unwrap_err();
        assert!(
            error.to_string().contains("expected KEY=VALUE"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn build_launch_plan_with_env_overrides_reach_native_session_resolution() {
        // The same override list is what `run.rs` also applies to the spawned
        // child's `Command`; proving it steers `session_dir` here proves
        // ai-memory's own native-session resolution and the harness process
        // agree on a caller-supplied `CLAUDE_CONFIG_DIR`, per the docs note
        // this closes (#820).
        let overrides = vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            "/accounts/work".to_string(),
        )];
        let plan = ai_memory_workstream::build_launch_plan_with_env(
            ManagedHarness::Claude,
            None,
            Vec::new(),
            None,
            &overrides,
            None,
        )
        .unwrap();
        assert_eq!(
            plan.session_dir.as_deref(),
            Some(Path::new("/accounts/work/projects"))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_claude_background_session_is_attached_to_not_resumed() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let project = temp
            .path()
            .join(".claude/projects")
            .join(cwd.to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&project).unwrap();
        let full = "94e41265-4001-431c-828e-1c54953e596b";
        let calls = temp.path().join("calls");
        let listing = temp.path().join("agents.json");
        let script = temp.path().join("claude");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"$*\" >> \"$FAKE_CALLS\"\ncat \"$FAKE_AGENTS\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = [
            ("FAKE_CALLS".to_string(), calls.display().to_string()),
            ("FAKE_AGENTS".to_string(), listing.display().to_string()),
        ];
        std::fs::write(
            &listing,
            serde_json::json!([{
                "id": "94e41265", "sessionId": full, "kind": "background",
                "status": "idle", "cwd": cwd,
            }])
            .to_string(),
        )
        .unwrap();
        let mut plan = build_launch_plan(
            ManagedHarness::Claude,
            Some(script.clone().into()),
            Vec::new(),
            Some(full),
        )
        .unwrap();
        plan.session_dir = None;
        let child_env = EffectiveChildEnv::for_tests(
            env.iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );
        let search = child_env.executable_search();
        let attach =
            || claude_background_attach_id(&plan, temp.path(), &cwd, &child_env, full, search);
        let transcript = |kind: Option<&str>| {
            let mut record = serde_json::json!({"type": "user", "sessionId": full, "cwd": cwd});
            if let Some(kind) = kind {
                record["sessionKind"] = serde_json::json!(kind);
            }
            std::fs::write(project.join(format!("{full}.jsonl")), format!("{record}\n")).unwrap();
        };

        // A foreground session keeps its resume without asking Claude at all.
        transcript(None);
        assert_eq!(attach().await, None);
        assert!(
            !calls.exists(),
            "claude agents ran for a foreground session"
        );

        transcript(Some("bg"));
        assert_eq!(attach().await.as_deref(), Some("94e41265"));
        assert_eq!(
            std::fs::read_to_string(&calls).unwrap().trim(),
            format!("agents --json --cwd {}", cwd.display())
        );

        // Stopped (no longer listed), or a listing Claude could not produce:
        // the native resume stays.
        std::fs::write(&listing, "[]").unwrap();
        assert_eq!(attach().await, None);
        std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
        assert_eq!(attach().await, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_env_reaches_the_spawned_child_and_overrides_env_file() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::commands::run_autowire::WireOverrides;
        use crate::config::Config;

        let app = Router::new()
            .route(
                "/workstream/runs",
                post(|| async {
                    axum::Json(PrepareManagedRunResponse {
                        workstream_id: WorkstreamId::new(),
                        workstream_name: "default".into(),
                        run_id: ManagedRunId::new(),
                        resolved_agent: None,
                        native_session_id: None,
                        source_cursor: None,
                        sync_after: 0,
                        sync_through: 0,
                        may_adopt_existing_session: false,
                        manifest_warning: None,
                    })
                }),
            )
            .route(
                "/workstream/runs/{run_id}/finish",
                post(|| async {
                    axum::Json(FinishManagedRunResponse {
                        imported_events: 0,
                        latest_sequence: 0,
                    })
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();

        let captured = repo.path().join("captured-env");
        let script = repo.path().join("capture-env-harness");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf 'FOO=%s\\nPROFILE_ONLY=%s\\nCLAUDE_CONFIG_DIR=%s\\n' \"$FOO\" \"$PROFILE_ONLY\" \"$CLAUDE_CONFIG_DIR\" > {}\nexit 0\n",
                captured.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let env_file = repo.path().join("run.env");
        std::fs::write(
            &env_file,
            "# comment\n\nFOO=from-file\nCLAUDE_CONFIG_DIR=/from/file\n",
        )
        .unwrap();

        let mut config = Config::load(None, Some(home.path().to_path_buf())).unwrap();
        config.data_dir = data.path().to_path_buf();
        config.home_dir = Some(home.path().to_string_lossy().into_owned());
        config.server_url = format!("http://{address}");
        config.run_autowire = false;
        config.run.profiles.insert(
            "work".into(),
            crate::config::RunProfile {
                env: [
                    ("FOO".into(), "from-profile".into()),
                    ("PROFILE_ONLY".into(), "from-profile".into()),
                    ("CLAUDE_CONFIG_DIR".into(), "/from/profile".into()),
                ]
                .into(),
            },
        );

        let args = RunArgs {
            workspace: Some("ws".into()),
            project: Some("proj".into()),
            workstream: None,
            new_workstream: None,
            executable: Some(script.clone()),
            yolo: false,
            true_yolo: false,
            jail: None,
            no_jail: false,
            fresh: false,
            force_unlock: false,
            no_autowire: true,
            require_server: false,
            profile: Some("work".into()),
            env: vec![("CLAUDE_CONFIG_DIR".to_string(), "/from/cli".to_string())],
            env_file: Some(env_file.clone()),
            harness: Some(RunHarnessChoice::Claude),
            native_args: vec![OsString::from("--version")],
        };

        let overrides = WireOverrides::default();
        let exit = run_from_with_wiring(&config, args, repo.path(), &overrides)
            .await
            .expect("managed passthrough run completes");
        assert_eq!(exit, 0, "the harmless child exits 0");

        let captured_env = std::fs::read_to_string(&captured).unwrap();
        assert!(
            captured_env.contains("FOO=from-file"),
            "--env-file must override the profile and reach the spawned child: {captured_env}"
        );
        assert!(
            captured_env.contains("PROFILE_ONLY=from-profile"),
            "a profile-only entry must reach the spawned child: {captured_env}"
        );
        assert!(
            captured_env.contains("CLAUDE_CONFIG_DIR=/from/cli"),
            "--env must override a same-key --env-file entry for the spawned child: {captured_env}"
        );

        server.abort();
    }

    // ----------------------------------------------------------------
    // Degraded offline launches (server unreachable)
    // ----------------------------------------------------------------

    /// A loopback address that nothing listens on: bind, capture, release.
    /// Rebinding races exist but are vanishingly rare under a test load.
    async fn closed_port_url() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{address}")
    }

    #[cfg(unix)]
    fn repo_hooks() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks")
    }

    /// A fake Claude executable: captures its environment and argv, writes
    /// the transcript the launcher waits for, and exits with `code`.
    #[cfg(unix)]
    fn capture_env_claude(repo: &Path, home: &Path, code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let script = repo.join("capture-harness");
        let transcripts = home
            .join(".claude/projects")
            .join(repo.to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&transcripts).unwrap();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nenv > \"$CAPTURED_ENV\"\nprintf '%s\\n' \"$@\" > \"$CAPTURED_ARGS\"\n\
                 while [ $# -gt 0 ]; do\n\
                 if [ \"$1\" = --session-id ]; then\n\
                 printf '{{\"sessionId\":\"%s\",\"cwd\":\"%s\"}}\\n' \"$2\" '{repo}' > '{dir}'/\"$2\".jsonl\n\
                 fi\nshift\ndone\nexit {code}\n",
                repo = repo.display(),
                dir = transcripts.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// The common offline-launch fixture: temp home/data/repo, a config whose
    /// server URL points at a closed port, and RunArgs pinned to the fake
    /// Claude executable.
    #[cfg(unix)]
    async fn offline_run_fixture(code: i32) -> (tempfile::TempDir, Config, RunArgs) {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&repo).unwrap();
        let script = capture_env_claude(&repo, &home, code);

        let mut config = Config::load(None, Some(temp.path().join("data"))).unwrap();
        config.data_dir = temp.path().join("data");
        config.home_dir = Some(home.to_string_lossy().into_owned());
        config.server_url = closed_port_url().await;
        config.run_autowire = false;

        let args = RunArgs {
            workspace: Some("ws".into()),
            project: Some("proj".into()),
            workstream: None,
            new_workstream: None,
            executable: Some(script),
            yolo: false,
            true_yolo: false,
            jail: None,
            no_jail: true,
            fresh: false,
            force_unlock: false,
            no_autowire: true,
            require_server: false,
            profile: None,
            env: Vec::new(),
            env_file: None,
            harness: Some(RunHarnessChoice::Claude),
            native_args: Vec::new(),
        };
        (temp, config, args)
    }

    /// The degraded launch spawns the child without any server, propagates
    /// its exact exit code, strips the run/workstream attribution ids from
    /// its environment, and still points its hooks at the configured server.
    #[cfg(unix)]
    #[tokio::test]
    async fn offline_launch_spawns_the_child_and_propagates_its_exit_code() {
        let (temp, config, mut args) = offline_run_fixture(7).await;
        let captured_env = temp.path().join("captured-env");
        let captured_args = temp.path().join("captured-args");
        args.env = vec![
            (
                "CAPTURED_ENV".to_string(),
                captured_env.display().to_string(),
            ),
            (
                "CAPTURED_ARGS".to_string(),
                captured_args.display().to_string(),
            ),
        ];

        let exit = run_from_with_wiring(
            &config,
            args,
            &temp.path().join("repo"),
            &WireOverrides::default(),
        )
        .await
        .expect("a degraded launch still runs the harness");

        assert_eq!(exit, 7, "the child's exit code is returned, not an error");
        let child_env_text = std::fs::read_to_string(&captured_env).unwrap();
        assert!(
            !child_env_text
                .lines()
                .any(|line| line.starts_with("AI_MEMORY_RUN_ID=")),
            "no run id may be attributed offline:\n{child_env_text}"
        );
        assert!(
            !child_env_text
                .lines()
                .any(|line| line.starts_with("AI_MEMORY_WORKSTREAM_ID=")),
            "no workstream id may be attributed offline:\n{child_env_text}"
        );
        assert!(
            child_env_text
                .lines()
                .any(|line| line.starts_with("AI_MEMORY_HOOK_URL=")),
            "the hook URL still points at the configured server:\n{child_env_text}"
        );
    }

    /// Without a server there is no lease, so no mutual exclusion: only an
    /// explicit native session selector resumes; a bare launch starts fresh.
    #[cfg(unix)]
    #[tokio::test]
    async fn offline_launch_resumes_only_with_an_explicit_session_selector() {
        let (temp, config, args) = offline_run_fixture(0).await;
        let repo = temp.path().join("repo");
        let captured_args =
            |name: &str| std::fs::read_to_string(temp.path().join(name)).unwrap_or_default();
        let run = |native_args: Vec<OsString>| async {
            let mut args = args.clone();
            args.native_args = native_args;
            args.env = vec![
                (
                    "CAPTURED_ENV".to_string(),
                    temp.path()
                        .join(format!("env-{}", args.native_args.len()))
                        .display()
                        .to_string(),
                ),
                (
                    "CAPTURED_ARGS".to_string(),
                    temp.path()
                        .join(format!("args-{}", args.native_args.len()))
                        .display()
                        .to_string(),
                ),
            ];
            run_from_with_wiring(&config, args, &repo, &WireOverrides::default()).await
        };

        run(Vec::new()).await.expect("bare offline launch");
        let bare = captured_args("args-0");
        assert!(
            bare.contains("--session-id"),
            "a bare offline launch starts a fresh session: {bare}"
        );

        run(vec![
            OsString::from("--resume"),
            OsString::from("explicit-session"),
        ])
        .await
        .expect("explicit-selector offline launch");
        let resumed = captured_args("args-2");
        assert!(
            resumed.contains("--resume")
                && resumed.contains("explicit-session")
                && !resumed.contains("--session-id"),
            "an explicit selector still resumes: {resumed}"
        );
    }

    /// Offline auto-wire keeps the hook install (capture must spool locally)
    /// but registers no MCP entry for the dead server and writes no sentinel.
    #[cfg(unix)]
    #[tokio::test]
    async fn offline_launch_wires_hooks_but_no_mcp_and_no_sentinel() {
        let (temp, mut config, mut args) = offline_run_fixture(0).await;
        config.run_autowire = true;
        let data = temp.path().join("data");
        let hooks_file = data.join("claude-settings.json");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(&hooks_file, "{}").unwrap();
        let mcp_file = data.join("claude.json");
        std::fs::write(&mcp_file, r#"{"existing":"keep"}"#).unwrap();
        let overrides = crate::commands::run_autowire::WireOverrides {
            hooks_dir: Some(repo_hooks()),
            hooks_config_file: Some(hooks_file.clone()),
            mcp_config_file: Some(mcp_file.clone()),
            ..Default::default()
        };
        args.no_autowire = false;
        // Keep the capture env names of the fixture off this launch.
        args.env.clear();

        run_from_with_wiring(&config, args, &temp.path().join("repo"), &overrides)
            .await
            .expect("offline launch with wiring");

        let hooks_json = std::fs::read_to_string(&hooks_file).unwrap();
        assert!(
            hooks_json.contains("ai-memory") || hooks_json.contains("ai_memory"),
            "hooks are still wired offline: {hooks_json}"
        );
        let mcp_json = std::fs::read_to_string(&mcp_file).unwrap();
        assert!(
            !mcp_json.contains("ai-memory"),
            "no dead-server MCP registration may be written: {mcp_json}"
        );
        assert!(
            std::fs::read_dir(crate::commands::run_autowire::autowire_state_dir(&data))
                .map(|entries| entries.count())
                .unwrap_or(0)
                == 0,
            "an offline half-wiring must not gate the next online launch"
        );
    }

    /// `--require-server` restores the fail-closed behavior with the same
    /// augmented diagnosis — including "the agent was not started" — and the
    /// harness never runs.
    #[cfg(unix)]
    #[tokio::test]
    async fn require_server_restores_the_fail_closed_error() {
        let (temp, config, mut args) = offline_run_fixture(0).await;
        args.require_server = true;

        let error = run_from_with_wiring(
            &config,
            args,
            &temp.path().join("repo"),
            &WireOverrides::default(),
        )
        .await
        .expect_err("require-server must fail closed");
        assert!(
            error.to_string().contains("the agent was not started"),
            "outer context matches the prepare failure: {error}"
        );
        let full = format!("{error:#}");
        assert!(
            full.contains("could not reach")
                && full.contains("/workstream/runs")
                && full.contains(&config.server_url),
            "the augmented connect diagnosis is reused verbatim: {full}"
        );
        assert!(
            !temp.path().join("repo").join("capture-harness").exists()
                || std::fs::read_to_string(temp.path().join("captured-args")).is_err(),
            "the harness must not run"
        );
    }

    /// A `config.toml`-level `run.require_server` behaves like the flag.
    #[cfg(unix)]
    #[tokio::test]
    async fn config_require_server_also_fails_closed() {
        let (temp, mut config, args) = offline_run_fixture(0).await;
        config.run.require_server = true;

        let error = run_from_with_wiring(
            &config,
            args,
            &temp.path().join("repo"),
            &WireOverrides::default(),
        )
        .await
        .expect_err("config require_server must fail closed");
        assert!(
            format!("{error:#}").contains("the agent was not started"),
            "{error:#}"
        );
    }

    /// The degraded-launch warning names everything the operator needs: the
    /// server URL, what is unavailable, what remains recoverable, the bounded
    /// retention, and the opt-outs.
    #[tokio::test]
    async fn offline_launch_warning_names_the_tradeoffs() {
        let url = closed_port_url().await;
        let endpoint = ServerEndpoint::from_config_resolving_auth(&Config {
            server_url: url,
            ..Config::load(None, None).unwrap()
        })
        .await;
        let ServerProbe::Unreachable(error) = probe_server(&endpoint).await else {
            panic!("closed port is unreachable");
        };
        let warning = offline_launch_warning(&endpoint, ManagedHarness::Claude, 8, error);

        assert!(warning.contains(&endpoint.url), "{warning}");
        assert!(warning.contains("could not reach"), "{warning}");
        assert!(warning.contains("Lost this run"), "{warning}");
        assert!(warning.contains("Still works"), "{warning}");
        assert!(warning.contains("spool"), "{warning}");
        assert!(warning.contains("ai-memory recover"), "{warning}");
        assert!(warning.contains("sanitized backfill"), "{warning}");
        assert!(warning.contains("8 failed drain passes"), "{warning}");
        assert!(
            warning.contains("AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS=0"),
            "{warning}"
        );
        assert!(warning.contains("--require-server"), "{warning}");
    }

    #[test]
    fn spooled_events_note_matches_the_status_phrasing() {
        use crate::commands::hook_spool::SpoolHealth;
        assert_eq!(
            spooled_events_note(&SpoolHealth::default()),
            "no hook events remain spooled locally"
        );
        let note = spooled_events_note(&SpoolHealth {
            pending: 3,
            oldest_age_ms: Some(45_000),
            retries_total: 1,
            ..SpoolHealth::default()
        });
        assert_eq!(note, "3 hook events remain spooled locally (oldest 45s)");
        let one = spooled_events_note(&SpoolHealth {
            pending: 1,
            oldest_age_ms: Some(0),
            retries_total: 0,
            ..SpoolHealth::default()
        });
        assert_eq!(one, "1 hook event remains spooled locally (oldest 0s)");
    }

    #[tokio::test]
    async fn unreachable_errors_are_transport_errors_not_server_answers() {
        let url = closed_port_url().await;
        let endpoint = ServerEndpoint::from_config_resolving_auth(&Config {
            server_url: url,
            ..Config::load(None, None).unwrap()
        })
        .await;
        let transport =
            post_json::<_, FinishManagedRunResponse>(&endpoint, "/workstream/runs/x/finish", &())
                .await
                .expect_err("closed port");
        assert!(is_unreachable_error(&transport));

        let answered = crate::http_client::server_response_error_for_test(
            reqwest::Method::POST,
            "/workstream/runs/x/finish",
            reqwest::StatusCode::BAD_REQUEST,
            "{\"error\":\"bad\"}".to_owned(),
        )
        .context("persisting the managed transcript; the native process has already exited");
        assert!(!is_unreachable_error(&answered));
    }

    #[test]
    fn offline_import_warning_points_at_recovery() {
        let warning = offline_import_warning(
            ManagedHarness::Claude,
            Some("session-123"),
            "ws",
            "proj",
            &anyhow!("connection refused"),
        );
        assert!(warning.contains("exit code is preserved"), "{warning}");
        assert!(warning.contains("ai-memory recover"), "{warning}");
        assert!(warning.contains("in ws/proj"), "{warning}");
        assert!(
            warning.contains(&SessionId::from_native("session-123").to_string()),
            "{warning}"
        );

        let generic = offline_import_warning(
            ManagedHarness::Crush,
            None,
            "ws",
            "proj",
            &anyhow!("timed out"),
        );
        assert!(generic.contains("session unknown"), "{generic}");
        assert!(generic.contains("ai-memory recover"), "{generic}");
    }

    /// A server that dies between prepare and finish: the child's exit code
    /// survives, the unrecorded run is reported, and no error propagates.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_mid_run_outage_after_the_child_exits_keeps_the_exit_code() {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let trigger = shutdown_tx.clone();
        let app = Router::new()
            .route(
                "/workstream/runs",
                post(|| async {
                    axum::Json(PrepareManagedRunResponse {
                        workstream_id: WorkstreamId::new(),
                        workstream_name: "default".into(),
                        run_id: ManagedRunId::new(),
                        resolved_agent: Some(AgentKind::ClaudeCode),
                        native_session_id: None,
                        source_cursor: None,
                        sync_after: 0,
                        sync_through: 0,
                        may_adopt_existing_session: false,
                        manifest_warning: None,
                    })
                }),
            )
            .route(
                "/workstream/runs/{run_id}/link",
                post(move || {
                    let _ = trigger.send(true);
                    async { StatusCode::NO_CONTENT }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let mut rx = shutdown_rx;
                    while rx.changed().await.is_ok() {
                        if *rx.borrow() {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
        });

        let (temp, mut config, args) = offline_run_fixture(5).await;
        config.server_url = format!("http://{address}");
        // Give the graceful shutdown time to close the listener before the
        // child exits and the finish POSTs begin.
        std::fs::write(
            args.executable.clone().unwrap(),
            std::fs::read_to_string(args.executable.clone().unwrap())
                .unwrap()
                .replace("exit 5", "sleep 1\nexit 5"),
        )
        .unwrap();

        let exit = run_from_with_wiring(
            &config,
            args,
            &temp.path().join("repo"),
            &WireOverrides::default(),
        )
        .await
        .expect("an outage after the child exits is a warning, not an error");
        assert_eq!(exit, 5, "the child's exit code is preserved");
        server.abort();
    }

    /// A store mismatch must stop before `/link` and before the harness
    /// spawns, release the prepared run, and leave the workstream link alone.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_store_mismatch_stops_before_linking_or_spawning_and_releases_the_run() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::commands::run_autowire::WireOverrides;
        use crate::config::Config;

        const LINKED: &str = "33333333-3333-4333-8333-333333333333";
        let cancels = Arc::new(AtomicUsize::new(0));
        let links = Arc::new(AtomicUsize::new(0));
        let cancel_hits = Arc::clone(&cancels);
        let link_hits = Arc::clone(&links);
        let app = Router::new()
            .route(
                "/workstream/runs",
                post(|| async {
                    axum::Json(PrepareManagedRunResponse {
                        workstream_id: WorkstreamId::new(),
                        workstream_name: "default".into(),
                        run_id: ManagedRunId::new(),
                        resolved_agent: None,
                        native_session_id: Some(LINKED.into()),
                        source_cursor: None,
                        sync_after: 0,
                        sync_through: 0,
                        may_adopt_existing_session: false,
                        manifest_warning: None,
                    })
                }),
            )
            .route(
                "/workstream/runs/{run_id}/cancel",
                post(move || {
                    cancel_hits.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::NO_CONTENT }
                }),
            )
            .route(
                "/workstream/runs/{run_id}/link",
                post(move || {
                    link_hits.fetch_add(1, Ordering::SeqCst);
                    async { StatusCode::NO_CONTENT }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let spawned = repo.path().join("spawned");
        let script = repo.path().join("marker-harness");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch {}\nexit 0\n", spawned.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut config = Config::load(None, Some(home.path().to_path_buf())).unwrap();
        config.data_dir = data.path().to_path_buf();
        config.home_dir = Some(home.path().to_string_lossy().into_owned());
        config.server_url = format!("http://{address}");
        config.run_autowire = false;

        let endpoint = ServerEndpoint::from_config_resolving_auth(&config).await;
        let custom = home.path().join("claude-glm").join("projects");
        let recorded = effective_store(
            ManagedHarness::Claude,
            home.path(),
            repo.path(),
            Some(&custom),
        );
        super::super::project_registry::record_session_store(
            &config, &endpoint, "claude", LINKED, &recorded,
        )
        .unwrap();

        let default_home = home.path().join(".claude");
        let args = RunArgs {
            workspace: Some("ws".into()),
            project: Some("proj".into()),
            workstream: None,
            new_workstream: None,
            executable: Some(script),
            yolo: false,
            true_yolo: false,
            jail: None,
            no_jail: false,
            fresh: false,
            force_unlock: false,
            no_autowire: true,
            require_server: false,
            profile: None,
            // Pinned so a contributor's own CLAUDE_CONFIG_DIR cannot leak in.
            env: vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                default_home.to_string_lossy().into_owned(),
            )],
            env_file: None,
            harness: Some(RunHarnessChoice::Claude),
            native_args: Vec::new(),
        };

        let error = run_from_with_wiring(&config, args, repo.path(), &WireOverrides::default())
            .await
            .expect_err("a session linked under another store must not be replaced");
        let message = format!("{error:#}");
        assert!(message.contains("--fresh"), "{message}");
        assert!(!spawned.exists(), "the harness must not start");
        assert_eq!(
            links.load(Ordering::SeqCst),
            0,
            "the workstream link must not move"
        );
        assert_eq!(
            cancels.load(Ordering::SeqCst),
            1,
            "the prepared run is released"
        );

        server.abort();
    }

    #[test]
    fn missing_linked_session_starts_fresh_but_explicit_selectors_win() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let session_root = temp.path().join("pi-sessions");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&session_root).unwrap();
        let transcript = session_root.join("linked.jsonl");
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({"type":"session","id":"linked","cwd":cwd})
            ),
        )
        .unwrap();
        let native_args = [
            OsString::from("--session-dir"),
            session_root.as_os_str().to_os_string(),
        ]
        .to_vec();
        let env = EffectiveChildEnv::default();

        let (resumed, orphaned) = build_preflighted_launch_plan(
            ManagedHarness::Pi,
            None,
            native_args.clone(),
            Some("linked"),
            false,
            temp.path(),
            &cwd,
            &env,
        )
        .unwrap();
        assert!(orphaned.is_none());
        assert!(resumed.args.iter().any(|arg| arg == "--session"));
        assert!(resumed.args.iter().any(|arg| arg == "linked"));

        std::fs::remove_file(transcript).unwrap();
        let (fresh, orphaned) = build_preflighted_launch_plan(
            ManagedHarness::Pi,
            None,
            native_args.clone(),
            Some("linked"),
            false,
            temp.path(),
            &cwd,
            &env,
        )
        .unwrap();
        assert_eq!(orphaned.as_deref(), Some("linked"));
        assert!(fresh.args.iter().any(|arg| arg == "--session-id"));
        assert!(!fresh.args.iter().any(|arg| arg == "linked"));

        let (explicit, orphaned) = build_preflighted_launch_plan(
            ManagedHarness::Pi,
            None,
            [
                native_args,
                [OsString::from("--session"), OsString::from("chosen")].to_vec(),
            ]
            .concat(),
            Some("linked"),
            false,
            temp.path(),
            &cwd,
            &env,
        )
        .unwrap();
        assert!(orphaned.is_none());
        assert_eq!(explicit.expected_session_id.as_deref(), Some("chosen"));
    }

    /// A linked session missing from this launch's store is only replaced
    /// when it was linked under the same store (or nothing was recorded).
    #[test]
    fn a_missing_session_linked_under_another_store_is_not_replaced() {
        let custom = PathBuf::from("/stores/claude-glm/projects");
        let default = PathBuf::from("/home/me/.claude/projects");
        let claude = ManagedHarness::Claude;

        assert!(store_mismatch_error(claude, "linked", None, &default).is_none());
        assert!(store_mismatch_error(claude, "linked", Some(custom.clone()), &custom).is_none());

        let error = store_mismatch_error(claude, "linked", Some(custom.clone()), &default)
            .expect("custom store recorded, default store resolved")
            .to_string();
        assert!(error.contains("/stores/claude-glm/projects"), "{error}");
        assert!(error.contains("/home/me/.claude/projects"), "{error}");
        assert!(error.contains("CLAUDE_CONFIG_DIR"), "{error}");
        assert!(error.contains("--fresh"), "{error}");

        let omp = store_mismatch_error(
            ManagedHarness::Omp,
            "linked",
            Some(custom.clone()),
            &default,
        )
        .expect("stores differ")
        .to_string();
        assert!(omp.contains("--profile"), "{omp}");
        let crush = store_mismatch_error(
            ManagedHarness::Crush,
            "linked",
            Some(custom.clone()),
            &default,
        )
        .expect("stores differ")
        .to_string();
        assert!(crush.contains("with the same --data-dir"), "{crush}");
        let command_code = store_mismatch_error(
            ManagedHarness::CommandCode,
            "linked",
            Some(custom),
            &default,
        )
        .expect("stores differ")
        .to_string();
        assert!(
            command_code.contains("relaunch from the original environment"),
            "{command_code}"
        );
    }

    /// The default store and an override naming the same directory are one
    /// store, however the override is spelled.
    #[test]
    fn effective_store_treats_equivalent_spellings_as_one_store() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let claude = ManagedHarness::Claude;
        let default = effective_store(claude, &home, &cwd, None);

        let explicit = home.join(".claude/projects");
        assert_eq!(
            effective_store(claude, &home, &cwd, Some(&explicit)),
            default
        );
        let dotted = home.join("x/../.claude/./projects/");
        assert_eq!(effective_store(claude, &home, &cwd, Some(&dotted)), default);
        let relative = Path::new("../home/.claude/projects");
        assert_eq!(
            effective_store(claude, &home, &cwd, Some(relative)),
            default
        );
        #[cfg(unix)]
        {
            let alias = temp.path().join("alias");
            std::os::unix::fs::symlink(home.join(".claude"), &alias).unwrap();
            assert_eq!(
                effective_store(claude, &home, &cwd, Some(&alias.join("projects"))),
                default
            );
        }

        let other = temp.path().join("claude-glm/projects");
        assert_ne!(effective_store(claude, &home, &cwd, Some(&other)), default);

        let later = temp.path().join("later/projects");
        let before = effective_store(claude, &home, &cwd, Some(&later));
        std::fs::create_dir_all(&later).unwrap();
        assert_eq!(effective_store(claude, &home, &cwd, Some(&later)), before);
    }

    /// Bare mode must scan the store the launch would resume from, not the
    /// default one, or a custom `CLAUDE_CONFIG_DIR` session is never found.
    #[tokio::test]
    async fn auto_session_scan_follows_the_store_override() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let cwd = temp.path().join("repo");
        let custom = temp.path().join("claude-custom");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        let write_session = |config_dir: &Path, id: &str| {
            // Flatten the whole path: on Windows the canonical cwd is an
            // absolute `\\?\C:\…` path, and joining it would escape the store.
            let bucket = config_dir.join("projects").join(
                cwd.to_string_lossy()
                    .replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
            );
            std::fs::create_dir_all(&bucket).unwrap();
            let line = serde_json::json!({
                "type": "user",
                "sessionId": id,
                "cwd": cwd,
                "message": {"role": "user", "content": "hi"},
            });
            std::fs::write(bucket.join(format!("{id}.jsonl")), format!("{line}\n")).unwrap();
        };
        let default_id = "11111111-1111-4111-8111-111111111111";
        let custom_id = "22222222-2222-4222-8222-222222222222";
        write_session(&home.join(".claude"), default_id);
        write_session(&custom, custom_id);
        let overrides = [(
            "CLAUDE_CONFIG_DIR".to_string(),
            custom.to_string_lossy().into_owned(),
        )];
        let env = EffectiveChildEnv::for_tests(
            overrides
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );

        let found = list_auto_sessions(&home, &cwd, &env, Some(ManagedHarness::OpenCode))
            .await
            .unwrap();
        let claude: Vec<&str> = found
            .iter()
            .filter(|candidate| candidate.harness == ManagedHarness::Claude)
            .map(|candidate| candidate.session.native_session_id.as_str())
            .collect();
        assert_eq!(claude, [custom_id]);
    }

    #[test]
    fn force_fresh_bypasses_an_existing_link_and_rejects_native_selectors() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let env = EffectiveChildEnv::default();
        let (fresh, orphaned) = build_preflighted_launch_plan(
            ManagedHarness::Claude,
            None,
            Vec::new(),
            Some("linked"),
            true,
            temp.path(),
            &cwd,
            &env,
        )
        .unwrap();
        assert!(orphaned.is_none());
        assert!(fresh.args.iter().any(|arg| arg == "--session-id"));
        assert!(!fresh.args.iter().any(|arg| arg == "--resume"));

        let error = build_preflighted_launch_plan(
            ManagedHarness::Claude,
            None,
            [OsString::from("--resume"), OsString::from("chosen")].to_vec(),
            Some("linked"),
            true,
            temp.path(),
            &cwd,
            &env,
        )
        .unwrap_err();
        assert!(error.to_string().contains("--fresh cannot be combined"));
    }

    #[tokio::test]
    async fn a_claude_run_that_attached_to_a_background_session_follows_it() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let project = temp.path().join(".claude/projects/-repo");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let started_at = SystemTime::now() - std::time::Duration::from_secs(60);
        let record = |session: &str, kind: Option<&str>| {
            let mut value = serde_json::json!({
                "type": "user",
                "sessionId": session,
                "session_id": "fg",
                "cwd": cwd,
            });
            if let Some(kind) = kind {
                value["sessionKind"] = serde_json::json!(kind);
            }
            format!("{value}\n")
        };
        std::fs::write(project.join("fg.jsonl"), record("fg", None)).unwrap();
        let mut plan =
            build_launch_plan(ManagedHarness::Claude, None, Vec::new(), Some("fg")).unwrap();
        // CLAUDE_CONFIG_DIR in the developer's environment would point the
        // scan away from the transcripts planted under `temp`.
        plan.session_dir = None;
        assert_eq!(plan.expected_session_id.as_deref(), Some("fg"));
        let resolve = |plan: &LaunchPlan| {
            let plan = plan.clone();
            let home = temp.path().to_path_buf();
            let cwd = cwd.clone();
            async move {
                resolve_native_session_after_run(
                    &plan,
                    ManagedHarness::Claude,
                    &home,
                    &cwd,
                    started_at,
                    None,
                    &Sanitizer::builtin(),
                )
                .await
                .unwrap()
            }
        };
        assert_eq!(resolve(&plan).await.as_deref(), Some("fg"));

        // `/resume` on a daemon-hosted session: the conversation continues in
        // that transcript, so the workstream must not stay on "fg".
        std::fs::write(project.join("bg.jsonl"), record("bg", Some("bg"))).unwrap();
        assert_eq!(resolve(&plan).await.as_deref(), Some("bg"));
    }

    #[tokio::test]
    async fn a_claude_run_that_was_cleared_follows_the_new_session() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let project = temp.path().join(".claude/projects/-repo");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let started_at = SystemTime::now() - std::time::Duration::from_secs(60);
        let turn = |session: &str| {
            serde_json::json!({
                "type": "user",
                "sessionId": session,
                "session_id": "launch",
                "cwd": cwd,
            })
        };
        std::fs::write(
            project.join("launch.jsonl"),
            format!("{}\n", turn("launch")),
        )
        .unwrap();
        let mut plan =
            build_launch_plan(ManagedHarness::Claude, None, Vec::new(), Some("launch")).unwrap();
        // CLAUDE_CONFIG_DIR in the developer's environment would point the
        // scan away from the transcripts planted under `temp`.
        plan.session_dir = None;
        let resolve = || {
            let plan = plan.clone();
            let home = temp.path().to_path_buf();
            let cwd = cwd.clone();
            async move {
                resolve_native_session_after_run(
                    &plan,
                    ManagedHarness::Claude,
                    &home,
                    &cwd,
                    started_at,
                    None,
                    &Sanitizer::builtin(),
                )
                .await
                .unwrap()
            }
        };
        assert_eq!(resolve().await.as_deref(), Some("launch"));

        // `/clear` inside the resumed session: the conversation goes on in a
        // new transcript, so the next launch must resume that one.
        let clear = serde_json::json!({
            "type": "user",
            "sessionId": "cleared",
            "cwd": cwd,
            "message": {"role": "user", "content": "<command-name>/clear</command-name>"},
        });
        std::fs::write(
            project.join("cleared.jsonl"),
            format!("{clear}\n{}\n", turn("cleared")),
        )
        .unwrap();
        assert_eq!(resolve().await.as_deref(), Some("cleared"));
    }

    /// A session linked during the run was reported by this run's child, so
    /// it wins over a newer session another launch made in the same checkout,
    /// even when it repeats the session the run was prepared with. A child's
    /// own descendants inherit the run id, so a linked session this checkout
    /// does not hold is set aside. Without a link, discovery still decides.
    #[tokio::test]
    async fn a_session_linked_during_the_run_wins_over_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let session_root = temp.path().join(".codex/sessions/2026/01/01");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&session_root).unwrap();
        let started_at = SystemTime::now();
        let rollout = |name: &str, id: &str, cwd: &Path| {
            std::fs::write(
                session_root.join(format!("rollout-{name}.jsonl")),
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "session_meta",
                        "payload": {"id": id, "cwd": cwd}
                    })
                ),
            )
            .unwrap();
        };
        rollout("prepared", "prepared", &cwd);
        rollout("nested", "nested", &temp.path().join("other-checkout"));
        rollout("concurrent", "concurrent-newer", &cwd);
        let mut plan = build_launch_plan(ManagedHarness::Codex, None, Vec::new(), None).unwrap();
        // The plan reads the process environment: a developer's CODEX_HOME
        // would send discovery away from the rollouts planted under `temp`.
        plan.session_dir = None;
        let status = |linked: bool, native: &str| ManagedRunStatus {
            run_id: ManagedRunId::new(),
            workstream_id: WorkstreamId::new(),
            agent: AgentKind::Codex,
            native_session_id: Some(native.to_string()),
            native_session_linked: linked,
            context_delivered: true,
            state: "active".to_string(),
        };
        for (linked, native, expected, own) in [
            (true, "prepared", Some("prepared"), Some("prepared")),
            (false, "prepared", Some("concurrent-newer"), None),
            (true, "nested", Some("concurrent-newer"), None),
        ] {
            let status = status(linked, native);
            // Only a session the run can prove is its own: never a discovered
            // one, which may be a concurrent launch's (#941 finalizes it).
            assert_eq!(
                own_native_session(
                    &plan,
                    ManagedHarness::Codex,
                    temp.path(),
                    &cwd,
                    Some(&status),
                    &Sanitizer::builtin(),
                )
                .unwrap()
                .as_deref(),
                own,
                "own: linked={linked} native={native}"
            );
            assert_eq!(
                resolve_native_session_after_run(
                    &plan,
                    ManagedHarness::Codex,
                    temp.path(),
                    &cwd,
                    started_at,
                    Some(&status),
                    &Sanitizer::builtin(),
                )
                .await
                .unwrap()
                .as_deref(),
                expected,
                "linked={linked} native={native}"
            );
        }
        // With nothing to discover here, the other checkout's session is not
        // taken as a fallback either; an unlinked report still is.
        let empty = temp.path().join("empty-checkout");
        std::fs::create_dir_all(&empty).unwrap();
        for (linked, expected) in [(true, None), (false, Some("nested"))] {
            let status = status(linked, "nested");
            assert_eq!(
                resolve_native_session_after_run(
                    &plan,
                    ManagedHarness::Codex,
                    temp.path(),
                    &empty,
                    started_at,
                    Some(&status),
                    &Sanitizer::builtin(),
                )
                .await
                .unwrap()
                .as_deref(),
                expected,
                "linked={linked}"
            );
        }
    }

    #[tokio::test]
    async fn utility_launch_does_not_adopt_a_recent_unrelated_session() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        let session_root = temp.path().join(".codex/sessions/2026/01/01");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&session_root).unwrap();
        let started_at = SystemTime::now();
        std::fs::write(
            session_root.join("rollout-current.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "type": "session_meta",
                    "payload": {"id": "unrelated-current", "cwd": cwd}
                })
            ),
        )
        .unwrap();

        let mut utility = build_launch_plan(
            ManagedHarness::Codex,
            None,
            vec![OsString::from("--version")],
            None,
        )
        .unwrap();
        // Both plans read the process environment. With a developer's
        // CODEX_HOME set, discovery would look away from the rollout planted
        // under `temp`, and the `is_none()` below would pass for that reason
        // instead of the passthrough one.
        utility.session_dir = None;
        assert_eq!(utility.mode, LaunchMode::Passthrough);
        assert!(
            resolve_native_session_after_run(
                &utility,
                ManagedHarness::Codex,
                temp.path(),
                &cwd,
                started_at,
                None,
                &Sanitizer::builtin(),
            )
            .await
            .unwrap()
            .is_none()
        );

        let mut session = build_launch_plan(ManagedHarness::Codex, None, Vec::new(), None).unwrap();
        session.session_dir = None;
        assert_eq!(
            resolve_native_session_after_run(
                &session,
                ManagedHarness::Codex,
                temp.path(),
                &cwd,
                started_at,
                None,
                &Sanitizer::builtin(),
            )
            .await
            .unwrap()
            .as_deref(),
            Some("unrelated-current")
        );
    }

    /// Two new Crush sessions in one store cannot be told apart, and that is
    /// reported without cancelling the finished run or falling back to the
    /// session the run was prepared with, which another launch may have
    /// moved. With no ambiguity a fresh run that created nothing keeps that
    /// session, and `--continue` claims the session it resumed.
    #[tokio::test]
    async fn ambiguous_crush_discovery_keeps_the_run() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let started = 1_900_000_000_i64;
        let started_at = SystemTime::UNIX_EPOCH + Duration::from_secs(started as u64);
        let store = |name: &str, sessions: &[(&str, i64, i64)]| {
            // Inside the checkout: a store only this project uses.
            let data = cwd.join(name);
            std::fs::create_dir_all(&data).unwrap();
            let connection = rusqlite::Connection::open(data.join("crush.db")).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE sessions(id TEXT PRIMARY KEY, parent_session_id TEXT, \
                     updated_at INTEGER NOT NULL, created_at INTEGER NOT NULL);",
                )
                .unwrap();
            for (id, created, updated) in sessions {
                connection
                    .execute(
                        "INSERT INTO sessions VALUES (?1, NULL, ?2, ?3)",
                        rusqlite::params![id, started + updated, started + created],
                    )
                    .unwrap();
            }
            data
        };
        let crowded = store(
            "crowded",
            &[("continued", -1_000, 30), ("a", 5, 20), ("b", 8, 10)],
        );
        let quiet = store("quiet", &[("continued", -1_000, 30)]);
        let status = ManagedRunStatus {
            run_id: ManagedRunId::new(),
            workstream_id: WorkstreamId::new(),
            agent: AgentKind::Crush,
            native_session_id: Some("prepared".to_string()),
            native_session_linked: false,
            context_delivered: false,
            state: "active".to_string(),
        };
        let resolve = async |data: &Path, extra: &[&str]| {
            let mut args = vec![OsString::from("--data-dir"), data.as_os_str().to_owned()];
            args.extend(extra.iter().map(OsString::from));
            let plan = build_launch_plan(ManagedHarness::Crush, None, args, None).unwrap();
            resolve_native_session_after_run(
                &plan,
                ManagedHarness::Crush,
                temp.path(),
                &cwd,
                started_at,
                Some(&status),
                &Sanitizer::builtin(),
            )
            .await
            .unwrap()
        };
        assert_eq!(resolve(&crowded, &[]).await, None);
        assert_eq!(resolve(&crowded, &["--continue"]).await, None);
        assert_eq!(resolve(&quiet, &[]).await.as_deref(), Some("prepared"));
        assert_eq!(
            resolve(&quiet, &["--continue"]).await.as_deref(),
            Some("continued")
        );
    }

    #[test]
    fn crush_context_config_preserves_user_settings_and_adds_packet() {
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("crush.json");
        std::fs::write(
            &source,
            serde_json::to_vec(&serde_json::json!({
                "options": {"debug": true, "global_context_paths": ["/existing.md"]},
                "providers": {"custom": {"type": "openai"}}
            }))
            .unwrap(),
        )
        .unwrap();

        let generated = write_crush_context_config(&source, "managed packet").unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(generated.path().join("crush.json")).unwrap())
                .unwrap();
        assert_eq!(config["options"]["debug"], true);
        assert_eq!(config["providers"]["custom"]["type"], "openai");
        let paths = config["options"]["global_context_paths"]
            .as_array()
            .unwrap();
        assert_eq!(paths[0], "/existing.md");
        let packet = paths[1].as_str().unwrap();
        assert_eq!(std::fs::read_to_string(packet).unwrap(), "managed packet");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opencode_executable_probe_selects_v2_for_launch_and_autowire() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::commands::run_autowire::WireOverrides;
        use crate::config::Config;

        let app = Router::new()
            .route(
                "/workstream/runs",
                post(|| async {
                    axum::Json(PrepareManagedRunResponse {
                        workstream_id: WorkstreamId::new(),
                        workstream_name: "default".into(),
                        run_id: ManagedRunId::new(),
                        resolved_agent: None,
                        native_session_id: None,
                        source_cursor: None,
                        sync_after: 0,
                        sync_through: 0,
                        may_adopt_existing_session: false,
                        manifest_warning: None,
                    })
                }),
            )
            .route(
                "/workstream/runs/{run_id}/finish",
                post(|| async {
                    axum::Json(FinishManagedRunResponse {
                        imported_events: 0,
                        latest_sequence: 0,
                    })
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let launched = repo.path().join("launched");
        let executable_dir = repo.path().join("child-bin");
        std::fs::create_dir_all(&executable_dir).unwrap();
        let executable = executable_dir.join("opencode");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then printf 'opencode v%s.0.23\\n' \"$OPENCODE_MAJOR\"; exit 0; fi\nprintf '%s|%s\\n' \"$*\" \"$OPENCODE_MAJOR\" > {}\n",
                launched.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

        let plugins = data.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let plugin = plugins.join("ai-memory-opencode2.ts");
        let mcp = data.path().join("opencode.json");
        let mut config = Config::load(None, Some(home.path().to_path_buf())).unwrap();
        config.data_dir = data.path().to_path_buf();
        config.home_dir = Some(home.path().to_string_lossy().into_owned());
        config.server_url = format!("http://{address}");
        config.run_autowire = true;
        let overrides = WireOverrides {
            hooks_config_file: Some(plugin.clone()),
            mcp_config_file: Some(mcp.clone()),
            ..WireOverrides::default()
        };
        let args = RunArgs {
            workspace: Some("ws".into()),
            project: Some("proj".into()),
            workstream: None,
            new_workstream: None,
            executable: None,
            profile: None,
            yolo: false,
            true_yolo: false,
            jail: None,
            no_jail: false,
            fresh: false,
            force_unlock: false,
            no_autowire: false,
            require_server: false,
            env: vec![
                ("PATH".into(), executable_dir.display().to_string()),
                ("OPENCODE_MAJOR".into(), "2".into()),
            ],
            env_file: None,
            harness: Some(RunHarnessChoice::OpenCode),
            native_args: vec![OsString::from("--help")],
        };

        let exit = run_from_with_wiring(&config, args, repo.path(), &overrides)
            .await
            .unwrap();
        assert_eq!(exit, 0);
        assert_eq!(
            std::fs::read_to_string(launched).unwrap().trim(),
            "--help|2"
        );
        assert!(
            std::fs::read_to_string(plugin)
                .unwrap()
                .contains("const AiMemoryOpencode2: Plugin.Plugin")
        );
        let mcp: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(mcp).unwrap()).unwrap();
        assert!(mcp["mcp"]["servers"]["ai-memory"].is_object());
        let sentinels = std::fs::read_dir(crate::commands::run_autowire::autowire_state_dir(
            data.path(),
        ))
        .unwrap()
        .count();
        assert_eq!(sentinels, 1);
        server.abort();
    }

    /// The `ai-memory run` -> autowire -> child-spawn seam: driving the launcher
    /// entry point (`run_from_with_wiring`) must run auto-wire *before* the child
    /// starts, so the harness's ai-memory hooks + MCP are installed and the
    /// auto-wire sentinel is written as a side effect of `run` itself.
    /// The autowire path injections keep it off the developer's real `$HOME`, the
    /// child is a harmless `exit 0` script launched in passthrough mode
    /// (`--version`), and a mock server stands in for the workstream endpoints, so
    /// nothing here needs a real editor, network, or LLM.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_entry_point_autowires_the_harness_before_spawning_the_child() {
        use std::os::unix::fs::PermissionsExt as _;

        use crate::commands::run_autowire::WireOverrides;
        use crate::config::Config;

        // Mock workstream server: prepare a run, accept the finish. Passthrough
        // launches never link a session, so these two routes are all `run_from`
        // touches over the wire.
        let app = Router::new()
            .route(
                "/workstream/runs",
                post(|| async {
                    axum::Json(PrepareManagedRunResponse {
                        workstream_id: WorkstreamId::new(),
                        workstream_name: "default".into(),
                        run_id: ManagedRunId::new(),
                        resolved_agent: None,
                        native_session_id: None,
                        source_cursor: None,
                        sync_after: 0,
                        sync_through: 0,
                        may_adopt_existing_session: false,
                        manifest_warning: None,
                    })
                }),
            )
            .route(
                "/workstream/runs/{run_id}/finish",
                post(|| async {
                    axum::Json(FinishManagedRunResponse {
                        imported_events: 0,
                        latest_sequence: 0,
                    })
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();

        // A harmless child the launcher can actually spawn: exits 0 immediately,
        // so the run completes without a real harness.
        let script = repo.path().join("harmless-harness");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        // Injected autowire targets — the whole point of the override seam is that
        // wiring never resolves (and writes to) the developer's real `$HOME`.
        let settings = data.path().join("claude-settings.json");
        std::fs::write(&settings, r#"{"existingUserKey":"keep me"}"#).unwrap();
        let mcp = data.path().join("claude.json");
        std::fs::write(&mcp, r#"{"existingMcpKey":"keep me too"}"#).unwrap();

        let mut config = Config::load(None, Some(home.path().to_path_buf())).unwrap();
        config.data_dir = data.path().to_path_buf();
        config.home_dir = Some(home.path().to_string_lossy().into_owned());
        config.server_url = format!("http://{address}");
        config.run_autowire = true;

        let overrides = WireOverrides {
            hooks_dir: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks")),
            hooks_config_file: Some(settings.clone()),
            mcp_config_file: Some(mcp.clone()),
            ..WireOverrides::default()
        };
        let args = || RunArgs {
            workspace: Some("ws".into()),
            project: Some("proj".into()),
            workstream: None,
            new_workstream: None,
            executable: Some(script.clone()),
            yolo: false,
            true_yolo: false,
            jail: None,
            no_jail: false,
            fresh: false,
            force_unlock: false,
            no_autowire: false,
            require_server: false,
            profile: None,
            env: Vec::new(),
            env_file: None,
            harness: Some(RunHarnessChoice::Claude),
            native_args: vec![OsString::from("--version")],
        };

        let exit = run_from_with_wiring(&config, args(), repo.path(), &overrides)
            .await
            .expect("managed passthrough run completes");
        assert_eq!(exit, 0, "the harmless child exits 0");

        // Auto-wire ran through the `run` entry point: hooks + MCP were installed
        // into the injected targets, and the sentinel recording the attempt exists.
        let hooks_json = std::fs::read_to_string(&settings).unwrap();
        assert!(
            hooks_json.contains("existingUserKey"),
            "unrelated user settings must be preserved: {hooks_json}"
        );
        assert!(
            hooks_json.contains("ai-memory") || hooks_json.contains("ai_memory"),
            "run must auto-install the ai-memory hook before spawning: {hooks_json}"
        );
        let mcp_json = std::fs::read_to_string(&mcp).unwrap();
        assert!(
            mcp_json.contains("existingMcpKey"),
            "unrelated MCP config must be preserved: {mcp_json}"
        );
        assert!(
            mcp_json.contains("ai-memory"),
            "run must auto-install the ai-memory MCP server before spawning: {mcp_json}"
        );
        let sentinels = std::fs::read_dir(crate::commands::run_autowire::autowire_state_dir(
            data.path(),
        ))
        .expect("autowire-state dir created by the run")
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
        assert!(
            sentinels
                .iter()
                .any(|name| name.starts_with("claude-code-")),
            "run must record the per-agent autowire sentinel: {sentinels:?}"
        );

        // A second launch is gated by that sentinel: the seam is idempotent, so
        // neither config file is rewritten.
        let before_hooks = std::fs::read(&settings).unwrap();
        let before_mcp = std::fs::read(&mcp).unwrap();
        run_from_with_wiring(&config, args(), repo.path(), &overrides)
            .await
            .expect("second managed passthrough run completes");
        assert_eq!(
            std::fs::read(&settings).unwrap(),
            before_hooks,
            "a gated re-launch must not rewrite hook config"
        );
        assert_eq!(
            std::fs::read(&mcp).unwrap(),
            before_mcp,
            "a gated re-launch must not rewrite MCP config"
        );

        server.abort();
    }

    /// A mock workstream server for launches driven through `run_from`: it
    /// prepares a run, linked to `native_session_id` when one is given, and
    /// accepts the link and the finish.
    #[cfg(unix)]
    async fn mock_workstream_server(
        native_session_id: Option<&'static str>,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route(
                "/workstream/runs",
                post(move || async move {
                    axum::Json(PrepareManagedRunResponse {
                        workstream_id: WorkstreamId::new(),
                        workstream_name: "default".into(),
                        run_id: ManagedRunId::new(),
                        resolved_agent: None,
                        native_session_id: native_session_id.map(str::to_owned),
                        source_cursor: None,
                        sync_after: 0,
                        sync_through: 0,
                        may_adopt_existing_session: false,
                        manifest_warning: None,
                    })
                }),
            )
            .route(
                "/workstream/runs/{run_id}/link",
                post(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/workstream/runs/{run_id}/finish",
                post(|| async {
                    axum::Json(FinishManagedRunResponse {
                        imported_events: 0,
                        latest_sequence: 0,
                    })
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (address, server)
    }

    /// Synthetic launcher evidence only: this does not assert which hooks a
    /// real Claude daemon emits. An early child exit must still finish the
    /// linked background session; an unavailable agents command keeps resume.
    #[cfg(unix)]
    #[tokio::test]
    async fn background_attach_early_exit_and_unavailable_agents_finish_linked_session() {
        use crate::commands::run_autowire::WireOverrides;
        use std::os::unix::fs::PermissionsExt as _;

        const SESSION: &str = "94e41265-4001-431c-828e-1c54953e596b";
        for available in [true, false] {
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let links = requests.clone();
            let finishes = requests.clone();
            let app = Router::new()
                .route(
                    "/workstream/runs",
                    post(|| async {
                        axum::Json(PrepareManagedRunResponse {
                            workstream_id: WorkstreamId::new(),
                            workstream_name: "default".into(),
                            run_id: ManagedRunId::new(),
                            resolved_agent: None,
                            native_session_id: Some(SESSION.into()),
                            source_cursor: None,
                            sync_after: 0,
                            sync_through: 0,
                            may_adopt_existing_session: false,
                            manifest_warning: None,
                        })
                    }),
                )
                .route(
                    "/workstream/runs/{run_id}/link",
                    post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                        let links = links.clone();
                        async move {
                            links.lock().unwrap().push(("link", body));
                            StatusCode::NO_CONTENT
                        }
                    }),
                )
                .route(
                    "/workstream/runs/{run_id}/finish",
                    post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                        let finishes = finishes.clone();
                        async move {
                            finishes.lock().unwrap().push(("finish", body));
                            axum::Json(FinishManagedRunResponse {
                                imported_events: 0,
                                latest_sequence: 0,
                            })
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let home = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            let cwd = repo.path().canonicalize().unwrap();
            let project = home
                .path()
                .join(".claude/projects")
                .join(cwd.to_string_lossy().replace('/', "-"));
            std::fs::create_dir_all(&project).unwrap();
            let record = serde_json::json!({"type": "user", "sessionId": SESSION, "sessionKind": "bg", "cwd": cwd});
            std::fs::write(
                project.join(format!("{SESSION}.jsonl")),
                format!("{record}\n"),
            )
            .unwrap();
            let calls = data.path().join("calls");
            let listing = data.path().join("agents.json");
            std::fs::write(&listing, serde_json::json!([{"id": "94e41265", "sessionId": SESSION, "kind": "background", "cwd": cwd}]).to_string()).unwrap();
            let script = data.path().join("fake-claude");
            std::fs::write(&script, format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$FAKE_CALLS\"\nif [ \"$1\" = agents ]; then\n  {}\nfi\nexit 23\n",
                if available { "cat \"$FAKE_AGENTS\"; exit 0" } else { "exit 1" }
            )).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let config = launch_config(home.path(), data.path(), address);
            let mut args = run_args(
                RunHarnessChoice::Claude,
                script,
                vec![
                    ("FAKE_CALLS".into(), calls.display().to_string()),
                    ("FAKE_AGENTS".into(), listing.display().to_string()),
                ],
                &["--model", "test-model"],
            );
            args.no_autowire = true;
            args.no_jail = true;
            let exit = run_from_with_wiring(&config, args, &cwd, &WireOverrides::default())
                .await
                .unwrap();
            assert_eq!(exit, 23, "early child exit is preserved");
            let calls = std::fs::read_to_string(calls).unwrap();
            let calls: Vec<_> = calls.lines().collect();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0], format!("agents --json --cwd {}", cwd.display()));
            if available {
                assert_eq!(
                    calls[1], "attach 94e41265",
                    "native flags do not reach attach"
                );
            } else {
                assert!(calls[1].contains(&format!("--resume {SESSION}")));
                assert!(calls[1].contains("--model test-model"));
            }
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].0, "link");
            assert_eq!(requests[1].0, "finish");
            for (_, request) in requests.iter() {
                assert_eq!(request["native_session_id"], SESSION);
            }
            assert_eq!(requests[1].1["exit_code"], 23);
            assert_eq!(requests[1].1["complete"], true);
            server.abort();
        }
    }

    #[cfg(unix)]
    fn capture_env_script(dir: &Path, variable: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt as _;

        let captured = dir.join("captured-env");
        let script = dir.join("capture-env-harness");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s' \"${{{variable}-unset}}\" > {}\nexit 0\n",
                captured.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script, captured)
    }

    fn launch_config(home: &Path, data: &Path, address: std::net::SocketAddr) -> Config {
        let mut config = Config::load(None, Some(home.to_path_buf())).unwrap();
        config.data_dir = data.to_path_buf();
        config.home_dir = Some(home.to_string_lossy().into_owned());
        config.server_url = format!("http://{address}");
        config.run_autowire = true;
        config
    }

    #[cfg(unix)]
    fn run_args(
        harness: RunHarnessChoice,
        executable: PathBuf,
        env: Vec<(String, String)>,
        native_args: &[&str],
    ) -> RunArgs {
        RunArgs {
            workspace: Some("ws".into()),
            project: Some("proj".into()),
            workstream: None,
            new_workstream: None,
            executable: Some(executable),
            yolo: false,
            true_yolo: false,
            jail: None,
            no_jail: false,
            fresh: false,
            force_unlock: false,
            no_autowire: false,
            require_server: false,
            profile: None,
            env,
            env_file: None,
            harness: Some(harness),
            native_args: native_args.iter().map(OsString::from).collect(),
        }
    }

    /// `run` must hand its `--env` to auto-wire, or a relocated config home
    /// launches without hooks or MCP. The guard refuses every install outside
    /// the test's data dir, so if `run` dropped the env nothing is written at
    /// all, least of all to the real home.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_entry_point_passes_run_env_to_autowire() {
        use crate::commands::run_autowire::WireOverrides;

        let (address, server) = mock_workstream_server(None).await;
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (script, captured) = capture_env_script(repo.path(), "CLAUDE_CONFIG_DIR");
        let claude_home = data.path().join("claude-home");
        std::fs::create_dir_all(&claude_home).unwrap();

        let config = launch_config(home.path(), data.path(), address);
        let overrides = WireOverrides {
            hooks_dir: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks")),
            confine_to: Some(data.path().to_path_buf()),
            ..WireOverrides::default()
        };
        let env = vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            claude_home.display().to_string(),
        )];
        let exit = run_from_with_wiring(
            &config,
            run_args(RunHarnessChoice::Claude, script, env, &["--version"]),
            repo.path(),
            &overrides,
        )
        .await
        .expect("managed passthrough run completes");
        assert_eq!(exit, 0);
        assert_eq!(
            std::fs::read_to_string(&captured).unwrap(),
            claude_home.display().to_string()
        );

        let settings = claude_home.join("settings.json");
        assert!(
            std::fs::read_to_string(&settings)
                .is_ok_and(|s| s.contains("ai-memory") || s.contains("ai_memory")),
            "hooks missing in {}",
            settings.display()
        );
        let mcp = claude_home.join(".claude.json");
        assert!(
            std::fs::read_to_string(&mcp).is_ok_and(|s| s.contains("ai-memory")),
            "MCP missing in {}",
            mcp.display()
        );

        server.abort();
    }

    /// Two accounts of one harness on one machine: each `--profile` must reach
    /// the child and auto-wire its own config home on its own first launch.
    /// The autowire sentinel is keyed by install target, so the second
    /// profile is not skipped as "already wired" by the first.
    #[cfg(unix)]
    #[tokio::test]
    async fn each_run_profile_launches_and_autowires_its_own_config_home() {
        use crate::commands::run_autowire::WireOverrides;

        let (address, server) = mock_workstream_server(None).await;
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (script, captured) = capture_env_script(repo.path(), "CLAUDE_CONFIG_DIR");
        let work_home = data.path().join("claude-work");
        let personal_home = data.path().join("claude-personal");
        for dir in [&work_home, &personal_home] {
            std::fs::create_dir_all(dir).unwrap();
        }

        let mut config = launch_config(home.path(), data.path(), address);
        for (name, dir) in [("work", &work_home), ("personal", &personal_home)] {
            config.run.profiles.insert(
                name.to_string(),
                crate::config::RunProfile {
                    env: [("CLAUDE_CONFIG_DIR".to_string(), dir.display().to_string())].into(),
                },
            );
        }
        let overrides = WireOverrides {
            hooks_dir: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks")),
            confine_to: Some(data.path().to_path_buf()),
            ..WireOverrides::default()
        };

        for (name, dir, other) in [
            ("work", &work_home, &personal_home),
            ("personal", &personal_home, &work_home),
        ] {
            let mut args = run_args(
                RunHarnessChoice::Claude,
                script.clone(),
                vec![],
                &["--version"],
            );
            args.profile = Some(name.to_string());
            let other_before = std::fs::read_dir(other).unwrap().count();
            let exit = run_from_with_wiring(&config, args, repo.path(), &overrides)
                .await
                .unwrap_or_else(|error| panic!("profile {name} run fails: {error:#}"));
            assert_eq!(exit, 0);
            assert_eq!(
                std::fs::read_to_string(&captured).unwrap(),
                dir.display().to_string(),
                "profile {name} must reach the spawned child"
            );
            let settings = dir.join("settings.json");
            assert!(
                std::fs::read_to_string(&settings)
                    .is_ok_and(|s| s.contains("ai-memory") || s.contains("ai_memory")),
                "profile {name}: hooks missing in {}",
                settings.display()
            );
            let mcp = dir.join(".claude.json");
            assert!(
                std::fs::read_to_string(&mcp).is_ok_and(|s| s.contains("ai-memory")),
                "profile {name}: MCP missing in {}",
                mcp.display()
            );
            assert_eq!(
                std::fs::read_dir(other).unwrap().count(),
                other_before,
                "profile {name} must not wire the other account's config home"
            );
        }

        server.abort();
    }

    /// A Kiro v3 session stored under the default home is resumed with
    /// `KIRO_HOME` removed from the child, so auto-wire must wire that default
    /// home. Wiring the `--env` home instead left the resumed session with no
    /// hooks and no MCP.
    #[cfg(unix)]
    #[tokio::test]
    async fn kiro_v3_default_store_resume_autowires_the_home_the_child_reads() {
        use crate::commands::run_autowire::WireOverrides;

        const SESSION: &str = "sess_c3774f9d-269e-40d1-aa02-2bb0c0817b4e";
        let (address, server) = mock_workstream_server(Some(SESSION)).await;
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (script, captured) = capture_env_script(repo.path(), "KIRO_HOME");

        let session_dir = home
            .path()
            .join(".kiro/sessions/checkout-fixture")
            .join(SESSION);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("session.json"),
            serde_json::json!({
                "schemaVersion": "1.0.0",
                "dataModelVersion": 1,
                "id": SESSION,
                "workspacePaths": [repo.path()],
                "createdAt": "2026-08-06T10:00:00Z",
                "lastModifiedAt": "2026-08-06T10:05:00Z",
                "agentMode": "vibe",
                "status": "idle"
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(session_dir.join("messages.jsonl"), "{}\n").unwrap();
        let custom = home.path().join("custom-kiro");
        std::fs::create_dir_all(custom.join("sessions")).unwrap();

        let config = launch_config(home.path(), data.path(), address);
        let overrides = WireOverrides {
            hooks_dir: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks")),
            confine_to: Some(home.path().to_path_buf()),
            ..WireOverrides::default()
        };
        let env = vec![("KIRO_HOME".to_string(), custom.display().to_string())];
        let exit = run_from_with_wiring(
            &config,
            run_args(RunHarnessChoice::Kiro, script, env, &["--v3"]),
            repo.path(),
            &overrides,
        )
        .await
        .expect("managed Kiro v3 resume completes");
        assert_eq!(exit, 0);
        assert_eq!(
            std::fs::read_to_string(&captured).unwrap(),
            "unset",
            "the default-store resume must drop KIRO_HOME from the child"
        );

        let kiro_default = home.path().join(".kiro");
        let hooks = kiro_default.join("hooks").join("ai-memory.json");
        assert!(
            std::fs::read_to_string(&hooks).is_ok_and(|s| s.contains("ai-memory")),
            "hooks missing in {}",
            hooks.display()
        );
        let mcp = kiro_default.join("settings").join("mcp.json");
        assert!(
            std::fs::read_to_string(&mcp).is_ok_and(|s| s.contains("ai-memory")),
            "MCP missing in {}",
            mcp.display()
        );
        assert!(
            !custom.join("hooks").exists() && !custom.join("settings").exists(),
            "the KIRO_HOME the child no longer reads must stay untouched"
        );

        server.abort();
    }

    #[test]
    fn blank_home_overrides_names_only_blank_home_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            blank_home_overrides(ManagedHarness::Claude, &env(&[("CLAUDE_CONFIG_DIR", "  ")])),
            ["CLAUDE_CONFIG_DIR"]
        );
        assert!(
            blank_home_overrides(ManagedHarness::Claude, &env(&[("CLAUDE_CONFIG_DIR", "/x")]))
                .is_empty()
        );
        assert!(blank_home_overrides(ManagedHarness::Claude, &env(&[])).is_empty());
        assert!(
            blank_home_overrides(ManagedHarness::Codex, &env(&[("CLAUDE_CONFIG_DIR", "")]))
                .is_empty(),
            "only the launched harness's own variables"
        );
        assert_eq!(
            blank_home_overrides(
                ManagedHarness::Pi,
                &env(&[
                    ("PI_CODING_AGENT_SESSION_DIR", ""),
                    ("PI_CODING_AGENT_DIR", "/x")
                ])
            ),
            ["PI_CODING_AGENT_SESSION_DIR"]
        );
        assert_eq!(
            blank_home_overrides(
                ManagedHarness::Omp,
                &env(&[
                    ("PI_CODING_AGENT_SESSION_DIR", ""),
                    ("PI_CODING_AGENT_DIR", "/x")
                ])
            ),
            ["PI_CODING_AGENT_SESSION_DIR"]
        );
        assert_eq!(
            blank_home_overrides(
                ManagedHarness::Omp,
                &env(&[("PI_CONFIG_DIR", " "), ("XDG_DATA_HOME", "")])
            ),
            ["PI_CONFIG_DIR", "XDG_DATA_HOME"]
        );
        assert_eq!(
            blank_home_overrides(
                ManagedHarness::Crush,
                &env(&[("CRUSH_GLOBAL_CONFIG", "  "), ("XDG_CONFIG_HOME", "/x")])
            ),
            ["CRUSH_GLOBAL_CONFIG"]
        );
        assert!(
            blank_home_overrides(ManagedHarness::Claude, &env(&[("XDG_CONFIG_HOME", "")]))
                .is_empty(),
            "Crush's config home is Crush's alone"
        );
    }

    /// The managed Crush context layers onto the global config the child
    /// would read: `--env` over ai-memory's own environment, blank as unset.
    /// A relative `CRUSH_GLOBAL_CONFIG` is read from the launch directory, as
    /// Crush reads it, so the generated config and `crushrc` do not point at
    /// paths the child would resolve from its temporary config dir.
    #[test]
    fn crush_context_source_is_anchored_at_the_launch_dir() {
        let home = Path::new("/home/user");
        let cwd = Path::new("/work/repo");
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            crush_context_source(home, cwd, env(&[("CRUSH_GLOBAL_CONFIG", "cfg")])),
            cwd.join("cfg").join("crush.json")
        );
        assert_eq!(
            crush_context_source(home, cwd, env(&[])),
            home.join(".config").join("crush").join("crush.json")
        );
    }

    #[test]
    fn crush_global_config_path_follows_the_launch_env() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        let home = Path::new("/home/me");
        let default = home.join(".config").join("crush").join("crush.json");
        assert_eq!(
            crush_global_config_path(
                home,
                env(&[
                    ("CRUSH_GLOBAL_CONFIG", "/team/crush"),
                    ("XDG_CONFIG_HOME", "/xdg")
                ])
            ),
            Path::new("/team/crush").join("crush.json")
        );
        assert_eq!(
            crush_global_config_path(home, env(&[("XDG_CONFIG_HOME", "/xdg")])),
            Path::new("/xdg").join("crush").join("crush.json")
        );
        assert_eq!(
            crush_global_config_path(
                home,
                env(&[("CRUSH_GLOBAL_CONFIG", "  "), ("XDG_CONFIG_HOME", "")])
            ),
            default,
            "blank counts as unset"
        );
        assert_eq!(crush_global_config_path(home, env(&[])), default);
    }

    /// Crush skips an empty config and reads `null` as unset, so neither may
    /// stop a managed launch.
    #[test]
    fn crush_context_config_accepts_what_crush_accepts() {
        let root = tempfile::tempdir().unwrap();
        for (name, content) in [
            ("empty", ""),
            ("null-options", r#"{"options": null}"#),
            (
                "null-paths",
                r#"{"options": {"global_context_paths": null}}"#,
            ),
            ("null", "null"),
        ] {
            let dir = root.path().join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let source = dir.join("crush.json");
            std::fs::write(&source, content).unwrap();
            let generated = write_crush_context_config(&source, "managed packet")
                .unwrap_or_else(|error| panic!("{name}: {error:#}"));
            let config: serde_json::Value = serde_json::from_slice(
                &std::fs::read(generated.path().join("crush.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(
                config["options"]["global_context_paths"]
                    .as_array()
                    .map(Vec::len),
                Some(3),
                "{name}: {config}"
            );
        }
    }

    /// Crush cleans the config path before reading it and deriving its default
    /// context files, so a `..` in `CRUSH_GLOBAL_CONFIG` moves neither.
    #[test]
    fn crush_default_context_files_follow_the_cleaned_config_path() {
        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("cfg");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("crush.json"), r#"{"marker": 1}"#).unwrap();
        // `missing` does not exist: only a cleaned path reaches the file.
        let source = config_dir.join("missing").join("..").join("crush.json");

        let generated = write_crush_context_config(&source, "managed packet").unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(generated.path().join("crush.json")).unwrap())
                .unwrap();
        let paths: Vec<&str> = config["options"]["global_context_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(Path::new(paths[0]), config_dir.join("CRUSH.md"));
        assert_eq!(Path::new(paths[1]), root.path().join("AGENTS.md"));
        assert_eq!(config["marker"], 1, "the user's config was read");
    }

    /// Crush only fills in its default `CRUSH.md` / `AGENTS.md` while
    /// `global_context_paths` is empty, so the packet must not displace the
    /// user's own files.
    #[test]
    fn crush_context_config_keeps_crush_default_context_files() {
        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("config").join("crush");
        std::fs::create_dir_all(&config_dir).unwrap();
        let source = config_dir.join("crush.json");
        std::fs::write(&source, r#"{"options": {"debug": true}}"#).unwrap();

        let generated = write_crush_context_config(&source, "managed packet").unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&std::fs::read(generated.path().join("crush.json")).unwrap())
                .unwrap();
        let paths: Vec<&str> = config["options"]["global_context_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect();
        assert_eq!(paths.len(), 3, "{paths:?}");
        assert_eq!(Path::new(paths[0]), config_dir.join("CRUSH.md"));
        assert_eq!(
            Path::new(paths[1]),
            root.path().join("config").join("AGENTS.md")
        );
        assert_eq!(std::fs::read_to_string(paths[2]).unwrap(), "managed packet");
    }

    /// Crush runs the `crushrc` beside its global config, and moving the
    /// config dir for the context packet would drop the user's. The generated
    /// dir sources it from its own directory, so relative `source` lines keep
    /// working, whatever the path contains.
    #[cfg(unix)]
    #[test]
    fn crush_context_config_carries_the_global_crushrc() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let config_dir = root.join("it's my crush");
        std::fs::create_dir_all(&config_dir).unwrap();
        let source = config_dir.join("crush.json");

        let generated = write_crush_context_config(&source, "managed packet").unwrap();
        assert!(!generated.path().join("crushrc").exists());

        std::fs::write(config_dir.join("crushrc"), "source ./extra\n").unwrap();
        std::fs::write(config_dir.join("extra"), "pwd > \"$OUT\"\n").unwrap();
        let generated = write_crush_context_config(&source, "managed packet").unwrap();
        let out = root.join("out");
        // Crush runs a crushrc from its own directory.
        let status = std::process::Command::new("bash")
            .arg(generated.path().join("crushrc"))
            .current_dir(generated.path())
            .env("OUT", &out)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&out).unwrap().trim_end(),
            config_dir.to_str().unwrap()
        );
    }

    /// The launched harness must not see a blank store override that session
    /// import and auto-wire treat as unset, or it reads a blank-named
    /// directory nothing else follows.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_blank_store_override_is_dropped_from_the_child() {
        use crate::commands::run_autowire::WireOverrides;

        let (address, server) = mock_workstream_server(None).await;
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (script, captured) = capture_env_script(repo.path(), "CLAUDE_CONFIG_DIR");
        let config = launch_config(home.path(), data.path(), address);
        let mut args = run_args(
            RunHarnessChoice::Claude,
            script,
            vec![("CLAUDE_CONFIG_DIR".to_string(), "   ".to_string())],
            &["--version"],
        );
        args.no_autowire = true;
        let exit = run_from_with_wiring(&config, args, repo.path(), &WireOverrides::default())
            .await
            .expect("managed passthrough run completes");
        assert_eq!(exit, 0);
        assert_eq!(std::fs::read_to_string(&captured).unwrap(), "unset");

        server.abort();
    }

    /// OMP ranks `--profile` above `OMP_PROFILE`, so auto-wire sees what the
    /// child will run with; everything else in `--env` passes through
    /// untouched.
    #[test]
    fn autowire_env_follows_what_the_child_runs_with() {
        let run_env = vec![
            ("OMP_PROFILE".to_string(), "other".to_string()),
            ("KIRO_HOME".to_string(), "/custom".to_string()),
            ("FOO".to_string(), "x".to_string()),
        ];
        let lookup = |env: &[(String, String)], name: &str| {
            let values: Vec<_> = env
                .iter()
                .filter(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .collect();
            values
        };
        // Only the run_env: no process environment leaks into the test.
        let only = |env: Vec<(String, String)>| {
            move |name: &str| {
                env.iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        let launch = only(run_env.clone());

        let args = [OsString::from("--profile"), OsString::from("work")];
        let omp = autowire_env(ManagedHarness::Omp, &run_env, &args, &launch);
        assert_eq!(lookup(&omp, "OMP_PROFILE"), ["work"]);
        assert_eq!(lookup(&omp, "KIRO_HOME"), ["/custom"]);
        assert_eq!(lookup(&omp, "FOO"), ["x"]);
        assert_eq!(
            autowire_env(ManagedHarness::Pi, &run_env, &args, &launch),
            run_env,
            "only OMP reads --profile"
        );
        assert_eq!(
            autowire_env(ManagedHarness::Omp, &run_env, &[], &launch),
            run_env
        );

        // `--profile default` under an environment a profiled parent OMP
        // exported: OMP drops the inherited agent dir, and so must auto-wire.
        let home = Path::new("/home/me");
        let inherited = vec![
            ("OMP_PROFILE".to_string(), "work".to_string()),
            (
                "PI_CODING_AGENT_DIR".to_string(),
                "/home/me/.omp/profiles/work/agent".to_string(),
            ),
            ("PI_CONFIG_DIR".to_string(), String::new()),
        ];
        let default_args = [OsString::from("--profile"), OsString::from("default")];
        let wired = autowire_env(
            ManagedHarness::Omp,
            &inherited,
            &default_args,
            &only(inherited.clone()),
        );
        assert_eq!(
            ai_memory_workstream::omp_agent_dir(home, None, only(wired)).unwrap(),
            home.join(".omp").join("agent")
        );
    }

    /// Harnesses without a native session-end hook get their session closed
    /// when the managed run ends (#941): the exact session is looked up by the
    /// stored id its native id maps to, and a synthetic session-end is posted.
    /// Harnesses with their own hook, and runs with no session, are left alone.
    #[tokio::test]
    async fn run_end_finalizes_the_session_of_a_hookless_harness() {
        use std::sync::Mutex;

        use axum::extract::{Query, State};
        use axum::routing::get;

        use crate::config::Config;

        #[derive(Default)]
        struct Seen {
            lookups: Vec<std::collections::HashMap<String, String>>,
            batches: Vec<serde_json::Value>,
        }
        let seen = Arc::new(Mutex::new(Seen::default()));
        let app = Router::new()
            .route(
                "/admin/open-sessions",
                get(
                    |State(seen): State<Arc<Mutex<Seen>>>,
                     Query(query): Query<std::collections::HashMap<String, String>>| async move {
                        let session_id = query.get("session_id").cloned().unwrap_or_default();
                        seen.lock().unwrap().lookups.push(query);
                        axum::Json(serde_json::json!({
                            "sessions": [{ "session_id": session_id, "cwd": "/tmp/repo" }]
                        }))
                    },
                ),
            )
            .route(
                "/hook/batch",
                post(
                    |State(seen): State<Arc<Mutex<Seen>>>, axum::Json(body): axum::Json<serde_json::Value>| async move {
                        seen.lock().unwrap().batches.push(body);
                        axum::Json(serde_json::json!({ "accepted": 1 }))
                    },
                ),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let mut config = Config::load(None, Some(home.path().to_path_buf())).unwrap();
        config.data_dir = data.path().to_path_buf();
        config.server_url = format!("http://{address}");

        assert!(
            finalize_hookless_session(
                &config,
                ManagedHarness::Claude,
                Some("claude-1"),
                "ws",
                "proj"
            )
            .await
            .is_none()
        );
        assert!(
            finalize_hookless_session(&config, ManagedHarness::Antigravity, None, "ws", "proj")
                .await
                .is_none()
        );
        assert!(
            seen.lock().unwrap().lookups.is_empty(),
            "nothing to finalize yet"
        );

        let (session, finalized) = finalize_hookless_session(
            &config,
            ManagedHarness::Antigravity,
            Some("agy-session-1"),
            "ws",
            "proj",
        )
        .await
        .expect("a hookless harness with its own session is finalized");
        let stored = SessionId::from_native("agy-session-1").to_string();
        assert_eq!(session.to_string(), stored);
        assert_eq!(finalized.unwrap(), vec![stored.clone()]);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.lookups.len(), 1, "one exact-session lookup");
        let lookup = &seen.lookups[0];
        assert_eq!(
            lookup.get("session_id"),
            Some(&stored),
            "looked up by the stored id"
        );
        assert_eq!(
            lookup.get("agent").map(String::as_str),
            Some("antigravity-cli")
        );
        assert_eq!(lookup.get("workspace").map(String::as_str), Some("ws"));
        assert_eq!(lookup.get("project").map(String::as_str), Some("proj"));
        assert_eq!(
            lookup.get("include_ended").map(String::as_str),
            Some("true"),
            "a resumed session that was already ended is ended again"
        );
        assert_eq!(seen.batches.len(), 1, "one synthetic session-end batch");
        assert!(
            seen.batches[0].to_string().contains(&stored),
            "the session-end targets that session: {}",
            seen.batches[0]
        );
        drop(seen);
        server.abort();
    }

    #[test]
    fn native_identity_run_preflight_refuses_before_resume_or_private_path() {
        let home = tempfile::tempdir().unwrap();
        for id in [
            "sk-abcdefghijklmnopqrstuvwx",
            "native\u{202e}tail",
            "native\u{200b}tail",
        ] {
            let result = build_preflighted_launch_plan(
                ManagedHarness::Codex,
                None,
                Vec::new(),
                Some(id),
                false,
                home.path(),
                home.path(),
                &EffectiveChildEnv::default(),
            );
            assert!(
                result.is_err(),
                "dirty server identity must be refused before resume"
            );
            assert!(!format!("{:#}", result.unwrap_err()).contains(id));
        }
        assert!(
            build_preflighted_launch_plan(
                ManagedHarness::Claude,
                None,
                Vec::new(),
                Some("sk-abcdefghijklmnopqrstuvwx"),
                true,
                home.path(),
                home.path(),
                &EffectiveChildEnv::default(),
            )
            .is_err(),
            "fresh mode must validate the original prepared identity before discarding it"
        );
        let (plan, _) = build_preflighted_launch_plan(
            ManagedHarness::Claude,
            None,
            Vec::new(),
            None,
            false,
            home.path(),
            home.path(),
            &EffectiveChildEnv::default(),
        )
        .unwrap();
        assert!(uuid::Uuid::parse_str(plan.expected_session_id.as_deref().unwrap()).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_identity_run_old_server_refusal_prevents_child_and_transport() {
        let (address, server) = mock_workstream_server(Some("private-vendor")).await;
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (script, captured) = capture_env_script(repo.path(), "AI_MEMORY_MANAGED_RUN");
        let mut config = launch_config(home.path(), data.path(), address);
        config.run_autowire = false;
        config.sanitize.extra_patterns.push("private-vendor".into());
        let result = run_from(
            &config,
            run_args(RunHarnessChoice::Claude, script, Vec::new(), &[]),
            repo.path(),
        )
        .await;
        assert!(
            result.is_err(),
            "old server dirty identity must refuse the actual launch"
        );
        assert!(!captured.exists(), "child must not start");
        assert!(!format!("{:#}", result.unwrap_err()).contains("private-vendor"));
        server.abort();
    }
    #[tokio::test]
    async fn native_identity_finish_transport_refuses_before_request_and_preserves_exact_control() {
        let requests = Arc::new(std::sync::Mutex::new(Vec::<FinishManagedRunRequest>::new()));
        let captured = requests.clone();
        let app = Router::new().route(
            "/workstream/runs/{id}/finish",
            post(
                move |axum::Json(request): axum::Json<FinishManagedRunRequest>| {
                    let captured = captured.clone();
                    async move {
                        captured.lock().unwrap().push(request);
                        axum::Json(FinishManagedRunResponse {
                            imported_events: 1,
                            latest_sequence: 1,
                        })
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let home = tempfile::tempdir().unwrap();
        let config = launch_config(home.path(), home.path(), address);
        let endpoint = ServerEndpoint::from_config_resolving_auth(&config).await;
        let sanitizer = Sanitizer::new(&ai_memory_core::SanitizeConfig {
            extra_patterns: vec!["private-vendor".into()],
            ..Default::default()
        })
        .unwrap();
        let transcript = |id: &str| ExportedTranscript {
            native_session_id: id.into(),
            source_cursor: Some("cursor".into()),
            events: Vec::new(),
            losses: Vec::new(),
        };
        for bad in ["sk-abcdefghijklmnopqrstuvwx", "private-vendor"] {
            assert!(
                import_batches(
                    &endpoint,
                    "/workstream/runs/test",
                    transcript(bad),
                    Default::default(),
                    Some(0),
                    &sanitizer,
                )
                .await
                .is_err(),
                "dirty finish identity must not reach transport"
            );
            assert!(requests.lock().unwrap().is_empty());
            let mut with_event = transcript("vendor-界-01");
            with_event.events.push(ai_memory_core::NewWorkstreamEvent {
                event_id: "e".into(),
                agent: AgentKind::Codex,
                native_session_id: bad.into(),
                source_record_id: None,
                kind: ai_memory_core::WorkstreamEventKind::Message,
                role: None,
                content: "shared history".into(),
                occurred_at: None,
                metadata: serde_json::json!({}),
            });
            assert!(
                import_batches(
                    &endpoint,
                    "/workstream/runs/test",
                    with_event,
                    Default::default(),
                    Some(0),
                    &sanitizer,
                )
                .await
                .is_err(),
                "dirty event identity must not reach transport"
            );
            assert!(requests.lock().unwrap().is_empty());
        }
        assert_eq!(
            import_batches(
                &endpoint,
                "/workstream/runs/test",
                transcript("vendor-界-01"),
                Default::default(),
                Some(0),
                &sanitizer,
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            requests.lock().unwrap()[0].native_session_id.as_deref(),
            Some("vendor-界-01")
        );
        assert_eq!(
            requests.lock().unwrap()[0].source_cursor.as_deref(),
            Some("cursor")
        );
        server.abort();
    }

    #[tokio::test]
    async fn native_identity_run_status_and_expected_refuse_before_discovery() {
        let home = tempfile::tempdir().unwrap();
        let mut plan = build_launch_plan(ManagedHarness::Codex, None, Vec::new(), None).unwrap();
        plan.session_dir = None;
        let sanitizer = Sanitizer::new(&ai_memory_core::SanitizeConfig {
            extra_patterns: vec!["private-vendor".into()],
            ..Default::default()
        })
        .unwrap();
        for bad in ["sk-abcdefghijklmnopqrstuvwx", "private-vendor"] {
            plan.expected_session_id = Some(bad.into());
            assert!(
                own_native_session(
                    &plan,
                    ManagedHarness::Codex,
                    home.path(),
                    home.path(),
                    None,
                    &sanitizer
                )
                .is_err(),
                "expected identity must refuse before use"
            );
            plan.expected_session_id = None;
            let status = ManagedRunStatus {
                run_id: ManagedRunId::new(),
                workstream_id: WorkstreamId::new(),
                agent: AgentKind::Codex,
                native_session_id: Some(bad.into()),
                native_session_linked: true,
                context_delivered: false,
                state: "active".into(),
            };
            assert!(
                own_native_session(
                    &plan,
                    ManagedHarness::Codex,
                    home.path(),
                    home.path(),
                    Some(&status),
                    &sanitizer,
                )
                .is_err(),
                "linked status must refuse before native IO"
            );
            assert!(
                resolve_native_session_after_run(
                    &plan,
                    ManagedHarness::Codex,
                    home.path(),
                    home.path(),
                    SystemTime::now(),
                    Some(&status),
                    &sanitizer,
                )
                .await
                .is_err(),
                "dirty old-server status must not fall back to discovery"
            );
            plan.expected_session_id = Some("vendor-界-01".into());
            assert_eq!(
                own_native_session(
                    &plan,
                    ManagedHarness::Codex,
                    home.path(),
                    home.path(),
                    None,
                    &sanitizer
                )
                .unwrap()
                .as_deref(),
                Some("vendor-界-01")
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_identity_run_configured_selector_refuses_before_link_or_child() {
        let (address, server) = mock_workstream_server(None).await;
        let home = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let (script, captured) = capture_env_script(repo.path(), "AI_MEMORY_MANAGED_RUN");
        let mut config = launch_config(home.path(), data.path(), address);
        config.run_autowire = false;
        config.sanitize.extra_patterns.push("private-vendor".into());
        let result = run_from(
            &config,
            run_args(
                RunHarnessChoice::Claude,
                script,
                Vec::new(),
                &["--session-id", "private-vendor"],
            ),
            repo.path(),
        )
        .await;
        assert!(
            result.is_err(),
            "configured selector must refuse before link and spawn"
        );
        assert!(!captured.exists());
        assert!(!format!("{:#}", result.unwrap_err()).contains("private-vendor"));
        server.abort();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_identity_run_configured_old_status_refuses_before_native_io_and_finish() {
        for id in ["private-vendor", "vendor-界-01"] {
            let finishes = Arc::new(std::sync::Mutex::new(Vec::<FinishManagedRunRequest>::new()));
            let captured_finishes = finishes.clone();
            let app = Router::new()
                .route(
                    "/workstream/runs",
                    post(|| async {
                        axum::Json(PrepareManagedRunResponse {
                            workstream_id: WorkstreamId::new(),
                            workstream_name: "default".into(),
                            run_id: ManagedRunId::new(),
                            resolved_agent: None,
                            native_session_id: None,
                            source_cursor: None,
                            sync_after: 0,
                            sync_through: 0,
                            may_adopt_existing_session: false,
                            manifest_warning: None,
                        })
                    }),
                )
                .route(
                    "/workstream/runs/{id}",
                    axum::routing::get(move || async move {
                        axum::Json(ManagedRunStatus {
                            run_id: ManagedRunId::new(),
                            workstream_id: WorkstreamId::new(),
                            agent: AgentKind::Codex,
                            native_session_id: Some(id.into()),
                            native_session_linked: false,
                            context_delivered: false,
                            state: "active".into(),
                        })
                    }),
                )
                .route(
                    "/workstream/runs/{id}/finish",
                    post(
                        move |axum::Json(request): axum::Json<FinishManagedRunRequest>| {
                            let captured_finishes = captured_finishes.clone();
                            async move {
                                captured_finishes.lock().unwrap().push(request);
                                axum::Json(FinishManagedRunResponse {
                                    imported_events: 0,
                                    latest_sequence: 0,
                                })
                            }
                        },
                    ),
                )
                .route(
                    "/workstream/runs/{id}/cancel",
                    post(|| async { StatusCode::NO_CONTENT }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let home = tempfile::tempdir().unwrap();
            let repo = tempfile::tempdir().unwrap();
            let (script, child_capture) = capture_env_script(repo.path(), "AI_MEMORY_MANAGED_RUN");
            let mut config = launch_config(home.path(), home.path(), address);
            config.run_autowire = false;
            config.sanitize.extra_patterns.push("private-vendor".into());
            let result = run_from(
                &config,
                run_args(RunHarnessChoice::Codex, script, Vec::new(), &[]),
                repo.path(),
            )
            .await;
            assert!(
                child_capture.exists(),
                "the status is returned after the child exits"
            );
            if id == "private-vendor" {
                assert!(
                    result.is_err(),
                    "configured old status must refuse before native IO and finish"
                );
                assert!(!format!("{:#}", result.unwrap_err()).contains(id));
                assert!(finishes.lock().unwrap().is_empty());
            } else {
                assert_eq!(result.unwrap(), 0);
                assert_eq!(
                    finishes.lock().unwrap()[0].native_session_id.as_deref(),
                    Some(id)
                );
            }
            server.abort();
        }
    }

    #[tokio::test]
    async fn native_identity_run_configured_discovery_refuses_before_export() {
        let sanitizer = Sanitizer::new(&ai_memory_core::SanitizeConfig {
            extra_patterns: vec!["private-vendor".into()],
            ..Default::default()
        })
        .unwrap();
        for id in ["private-vendor", "vendor-界-01"] {
            let home = tempfile::tempdir().unwrap();
            let cwd = home.path().join("repo");
            let root = home.path().join(".codex/sessions/2026/01/01");
            std::fs::create_dir_all(&cwd).unwrap();
            std::fs::create_dir_all(&root).unwrap();
            let started_at = SystemTime::now();
            std::fs::write(
                root.join("rollout-current.jsonl"),
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type": "session_meta", "payload": {"id": id, "cwd": cwd}
                    })
                ),
            )
            .unwrap();
            let mut plan =
                build_launch_plan(ManagedHarness::Codex, None, Vec::new(), None).unwrap();
            plan.session_dir = None;
            let result = resolve_native_session_after_run(
                &plan,
                ManagedHarness::Codex,
                home.path(),
                &cwd,
                started_at,
                None,
                &sanitizer,
            )
            .await;
            if id == "private-vendor" {
                assert!(
                    result.is_err(),
                    "configured discovered identity must refuse before export"
                );
                assert!(!format!("{:#}", result.unwrap_err()).contains(id));
            } else {
                assert_eq!(result.unwrap().as_deref(), Some(id));
            }
        }
    }
}
