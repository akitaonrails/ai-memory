//! Opt-in managed cross-harness launcher.

use std::ffi::{OsStr, OsString};
use std::io::{self, IsTerminal as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use ai_memory_core::{
    AgentKind, FinishManagedRunRequest, FinishManagedRunResponse, LinkManagedRunRequest,
    ManagedRunContextResponse, ManagedRunStatus, PrepareManagedRunRequest,
    PrepareManagedRunResponse,
};
use ai_memory_workstream::{
    ExportedTranscript, LaunchMode, LaunchPlan, ManagedHarness, NativeSessionCandidate,
    allows_native_session_adoption, apply_yolo, build_launch_plan, discover_native_session,
    export_transcript, has_native_session_selector, inspect_repository, kiro_explicit_session_id,
    kiro_harness_from_source_cursor, kiro_selects_non_default_engine, kiro_selects_v2_engine,
    kiro_selects_v3_engine, kiro_v3_resume_uses_default_store, list_native_sessions,
    native_session_exists, wait_for_transcript_flush,
};
use anyhow::{Context as _, Result, anyhow};
use tokio::process::Command;

use crate::cli::{RunArgs, RunHarnessChoice};
use crate::commands::{path_util, resolve_scope};
use crate::config::Config;
use crate::http_client::{
    ServerEndpoint, ServerResponseError, get_json, post_empty, post_json, post_json_no_content,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const HEARTBEAT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const PREPARE_BUSY_RETRY_WINDOW: Duration = Duration::from_secs(5);
const PREPARE_BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const IMPORT_BATCH_EVENTS: usize = 400;
const IMPORT_BATCH_BYTES: usize = 1024 * 1024;
const ADOPTION_CANDIDATE_LIMIT: usize = 8;
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

/// Run one native harness and return its exact process exit code.
pub async fn run(config: &Config, args: RunArgs) -> Result<i32> {
    let cwd = std::env::current_dir().context("getting managed run working directory")?;
    run_from(config, args, &cwd).await
}

/// Run one native harness from an explicit checkout without changing the
/// parent process's working directory.
pub(super) async fn run_from(config: &Config, args: RunArgs, cwd: &Path) -> Result<i32> {
    let repository = inspect_repository(cwd)?;
    let home = native_home(config).context("locating native harness session storage")?;
    let automatic_harness = args.harness.is_none();
    let mut native_args = args.native_args;
    let trailing_yolo = remove_wrapper_yolo(&mut native_args);
    let trailing_fresh = remove_wrapper_fresh(&mut native_args);
    let force_fresh = args.fresh || trailing_fresh;
    if automatic_harness && !native_args.is_empty() {
        return Err(anyhow!(
            "native harness arguments require an explicit harness; try `ai-memory run codex ...`"
        ));
    }
    if automatic_harness && args.executable.is_some() {
        return Err(anyhow!(
            "--executable requires an explicit harness; try `ai-memory run --executable <path> codex`"
        ));
    }
    let auto_candidates = if automatic_harness {
        filter_usable_auto_sessions(
            list_auto_sessions(&home, &repository.cwd).await?,
            |harness| executable_available(OsStr::new(harness.executable())),
        )?
    } else {
        Vec::new()
    };
    let provisional_harness = match args.harness {
        Some(choice) => managed_harness_for_args(choice, &native_args),
        None => auto_candidates
            .first()
            .map(|candidate| candidate.harness)
            .ok_or_else(no_auto_session_error)?,
    };
    let executable = args.executable.map(PathBuf::into_os_string);
    ensure_executable_available(provisional_harness, executable.as_deref())?;
    // Resolve BOTH halves here. `--workspace` used to default to a literal
    // `default`, so a checkout whose marker declared another workspace put its
    // managed workstream in one scope while its hook-captured sessions went to
    // another — the same repository split in two.
    let (workspace, project) =
        resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let may_adopt_native_session = args.new_workstream.is_none() && !force_fresh;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let prepare = PrepareManagedRunRequest {
        workspace: workspace.clone(),
        project: project.clone(),
        cwd: repository.cwd.to_string_lossy().into_owned(),
        repo_fingerprint: repository.repo_fingerprint,
        worktree_fingerprint: repository.worktree_fingerprint,
        agent: provisional_harness.agent_kind(),
        automatic_harness,
        available_agents: unique_auto_agents(&auto_candidates),
        workstream: args.workstream,
        new_workstream: args.new_workstream,
        lease_owner: lease_owner(),
    };
    let interrupted_before_spawn = Arc::new(AtomicBool::new(false));
    let interrupt_task = tokio::spawn(capture_interrupts(Arc::clone(&interrupted_before_spawn)));
    let prepared = prepare_managed_run(&endpoint, &prepare)
        .await
        .context("opening managed workstream; the agent was not started");
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            interrupt_task.abort();
            return Err(error);
        }
    };
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
    if interrupted_before_spawn.load(Ordering::SeqCst) {
        acquired_try!(Err(anyhow!(
            "managed run interrupted before the agent started"
        )));
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
    acquired_try!(ensure_executable_available(harness, executable.as_deref()));
    let native_grok_rules = user_supplied_grok_rules(&native_args);
    let (mut plan, orphaned_session) = acquired_try!(build_preflighted_launch_plan(
        harness,
        executable.clone(),
        native_args.clone(),
        prepared.native_session_id.as_deref(),
        force_fresh,
        &home,
        &repository.cwd,
    ));
    if let Some(orphaned_session) = orphaned_session {
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
        plan = acquired_try!(build_launch_plan(
            harness,
            executable.clone(),
            native_args.clone(),
            Some(&candidate.session.native_session_id),
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
                    )
                    .await
                );
                match selection {
                    Ok(Some(native_session_id)) => {
                        plan = acquired_try!(build_launch_plan(
                            harness,
                            executable,
                            native_args,
                            Some(&native_session_id),
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
    if args.yolo || trailing_yolo {
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
    if plan.mode == LaunchMode::Session
        && let Some(native_session_id) = &plan.expected_session_id
    {
        acquired_try!(
            post_json_no_content(
                &endpoint,
                &format!("{run_path}/link"),
                &LinkManagedRunRequest {
                    native_session_id: native_session_id.clone(),
                },
            )
            .await
            .context("linking the managed native session; the agent was not started")
        );
    }

    let crush_context = if harness == ManagedHarness::Crush && plan.mode == LaunchMode::Session {
        acquired_try!(prepare_crush_context(&endpoint, &run_path, &home).await)
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
    if interrupted_before_spawn.load(Ordering::SeqCst) {
        acquired_try!(Err(anyhow!(
            "managed run interrupted before the agent started"
        )));
    }

    let started_at = SystemTime::now();
    // Spawn the resolved file rather than the bare name: on Windows the name
    // alone can match an unlaunchable extension-less shim (see
    // `resolve_program`). Falling back to the plan's own value keeps an
    // unresolvable program reaching the spawn error below, which explains it.
    let program = resolve_program(&plan.program).unwrap_or_else(|| plan.program.clone().into());
    let mut command = Command::new(&program);
    command
        .args(&plan.args)
        .current_dir(&repository.cwd)
        .env("AI_MEMORY_RUN_ID", prepared.run_id.to_string())
        .env(
            "AI_MEMORY_WORKSTREAM_ID",
            prepared.workstream_id.to_string(),
        )
        .env("AI_MEMORY_HOOK_URL", endpoint.build_url(""))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
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
    let native_session_id = acquired_try!(
        resolve_native_session_after_run(
            &plan,
            harness,
            &home,
            &repository.cwd,
            started_at,
            server_status.as_ref(),
        )
        .await
    );
    let transcript = if plan.mode == LaunchMode::Session {
        let source_cursor = if native_session_id.as_deref() == prepared.native_session_id.as_deref()
        {
            prepared.source_cursor.as_deref()
        } else {
            None
        };
        export_after_flush(
            harness,
            &home,
            &repository.cwd,
            plan.session_dir.as_deref(),
            native_session_id.as_deref(),
            source_cursor,
        )
        .await
    } else {
        ExportedTranscript::default()
    };
    let checkpoint = inspect_repository(&repository.cwd)
        .map(|identity| identity.checkpoint)
        .unwrap_or(repository.checkpoint);
    let imported = acquired_try!(
        import_batches(
            &endpoint,
            &run_path,
            transcript,
            checkpoint,
            Some(exit_code),
        )
        .await
    );

    if plan.mode == LaunchMode::Session
        && prepared.sync_through > prepared.sync_after
        && !server_status.is_some_and(|status| status.context_delivered)
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
    Ok(exit_code)
}

async fn capture_interrupts(interrupted: Arc<AtomicBool>) {
    while tokio::signal::ctrl_c().await.is_ok() {
        interrupted.store(true, Ordering::SeqCst);
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

async fn resolve_native_session_after_run(
    plan: &LaunchPlan,
    harness: ManagedHarness,
    home: &Path,
    cwd: &Path,
    started_at: SystemTime,
    server_status: Option<&ManagedRunStatus>,
) -> Result<Option<String>> {
    if plan.mode == LaunchMode::Passthrough {
        return Ok(None);
    }
    if let Some(native_session_id) = &plan.expected_session_id {
        return Ok(Some(native_session_id.clone()));
    }
    let discovered =
        discover_native_session(harness, home, cwd, plan.session_dir.as_deref(), started_at)
            .await?;
    Ok(discovered.or_else(|| server_status.and_then(|status| status.native_session_id.clone())))
}

async fn list_auto_sessions(home: &Path, cwd: &Path) -> Result<Vec<AutoSessionCandidate>> {
    let mut found = Vec::new();
    let mut failures = Vec::new();
    for harness in AUTO_HARNESSES {
        match list_native_sessions(harness, home, cwd, None, 1).await {
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
) -> ManagedHarness {
    if selected == ManagedHarness::Kiro
        && provisional.agent_kind() == AgentKind::KiroCli
        && linked_session_id.is_none()
    {
        provisional
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

fn remove_wrapper_fresh(args: &mut Vec<OsString>) -> bool {
    let before = args.len();
    args.retain(|arg| arg != OsStr::new("--fresh"));
    args.len() != before
}

fn build_preflighted_launch_plan(
    harness: ManagedHarness,
    executable: Option<OsString>,
    native_args: Vec<OsString>,
    linked_session_id: Option<&str>,
    force_fresh: bool,
    home: &Path,
    cwd: &Path,
) -> Result<(LaunchPlan, Option<String>)> {
    let explicit_selector = has_native_session_selector(harness, &native_args);
    if force_fresh && explicit_selector {
        return Err(anyhow!(
            "--fresh cannot be combined with a native session, resume, continue, or fork selector"
        ));
    }
    let linked_session_id = if force_fresh { None } else { linked_session_id };
    let plan = build_launch_plan(
        harness,
        executable.clone(),
        native_args.clone(),
        linked_session_id,
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
            build_launch_plan(harness, executable, native_args, None)?,
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

fn ensure_executable_available(harness: ManagedHarness, executable: Option<&OsStr>) -> Result<()> {
    let program = executable.unwrap_or_else(|| OsStr::new(harness.executable()));
    if executable_available(program) {
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
pub(super) fn harness_available(choice: RunHarnessChoice) -> bool {
    executable_available(OsStr::new(managed_harness(choice).executable()))
}

fn executable_available(program: &OsStr) -> bool {
    resolve_program(program).is_some()
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
pub(super) fn resolve_program(program: &OsStr) -> Option<std::path::PathBuf> {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return resolve_candidate(path);
    }
    std::env::var_os("PATH").and_then(|path_value| {
        std::env::split_paths(&path_value).find_map(|dir| resolve_candidate(&dir.join(path)))
    })
}

/// Concrete launchable file for one candidate location.
fn resolve_candidate(path: &Path) -> Option<std::path::PathBuf> {
    #[cfg(windows)]
    {
        // An explicit extension is taken at face value; otherwise only a
        // PATHEXT match counts. Never the extension-less sibling.
        if path.extension().is_some() && executable_file(path) {
            return Some(path.to_path_buf());
        }
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
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
    home: &Path,
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

    write_crush_context_config(&crush_global_config_path(home), &context).map(Some)
}

fn write_crush_context_config(source: &Path, context: &str) -> Result<tempfile::TempDir> {
    let temp = tempfile::Builder::new()
        .prefix("ai-memory-crush-")
        .tempdir()
        .context("creating the temporary Crush context directory")?;
    let context_path = temp.path().join("managed-workstream.md");
    write_private(&context_path, context.as_bytes())?;

    let mut config = if source.is_file() {
        let raw = std::fs::read(source)
            .with_context(|| format!("reading Crush config {}", source.display()))?;
        serde_json::from_slice::<serde_json::Value>(&raw)
            .with_context(|| format!("parsing Crush config {}", source.display()))?
    } else {
        serde_json::json!({})
    };
    let root = config
        .as_object_mut()
        .context("Crush global config must be a JSON object")?;
    let options = root
        .entry("options")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("Crush global config `options` must be a JSON object")?;
    let paths = options
        .entry("global_context_paths")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .context("Crush `options.global_context_paths` must be an array")?;
    let context_path = context_path.to_string_lossy().into_owned();
    if !paths
        .iter()
        .any(|value| value.as_str() == Some(&context_path))
    {
        paths.push(serde_json::Value::String(context_path));
    }
    let rendered = serde_json::to_vec_pretty(&config).context("rendering Crush config")?;
    write_private(&temp.path().join("crush.json"), &rendered)?;
    Ok(temp)
}

fn crush_global_config_path(home: &Path) -> PathBuf {
    if let Some(dir) = std::env::var_os("CRUSH_GLOBAL_CONFIG").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir).join("crush.json");
    }
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(dir).join("crush/crush.json");
    }
    home.join(".config/crush/crush.json")
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

fn native_home(config: &Config) -> Option<PathBuf> {
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
) -> Result<io::Result<Option<String>>> {
    let mut chooser = tokio::task::spawn_blocking(move || {
        let stdin = io::stdin();
        let stderr = io::stderr();
        choose_native_session(
            harness,
            &workstream_name,
            &candidates,
            &mut stdin.lock(),
            &mut stderr.lock(),
            SystemTime::now(),
        )
    });
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

async fn import_batches(
    endpoint: &ServerEndpoint,
    run_path: &str,
    transcript: ExportedTranscript,
    checkpoint: ai_memory_core::WorkstreamCheckpoint,
    exit_code: Option<i32>,
) -> Result<usize> {
    let mut imported = 0;
    let mut batches = event_batches(transcript.events).into_iter().peekable();
    while let Some(batch) = batches.next() {
        let complete = batches.peek().is_none();
        let request = FinishManagedRunRequest {
            native_session_id: nonempty_session(&transcript.native_session_id),
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

async fn prepare_managed_run(
    endpoint: &ServerEndpoint,
    request: &PrepareManagedRunRequest,
) -> Result<PrepareManagedRunResponse> {
    prepare_managed_run_with_retry(
        endpoint,
        request,
        PREPARE_BUSY_RETRY_WINDOW,
        PREPARE_BUSY_RETRY_INTERVAL,
    )
    .await
}

async fn prepare_managed_run_with_retry(
    endpoint: &ServerEndpoint,
    request: &PrepareManagedRunRequest,
    retry_window: Duration,
    retry_interval: Duration,
) -> Result<PrepareManagedRunResponse> {
    let deadline = tokio::time::Instant::now() + retry_window;
    let mut reported_wait = false;
    loop {
        match post_json(endpoint, "/workstream/runs", request).await {
            Ok(response) => return Ok(response),
            Err(error)
                if is_active_workstream_conflict(&error)
                    && tokio::time::Instant::now() < deadline =>
            {
                if !reported_wait {
                    eprintln!(
                        "ai-memory: another launcher owns this workstream; waiting briefly in case it is finalizing"
                    );
                    reported_wait = true;
                }
                tokio::time::sleep(retry_interval).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_active_workstream_conflict(error: &anyhow::Error) -> bool {
    let Some(response) = error.downcast_ref::<ServerResponseError>() else {
        return false;
    };
    if response.status() != reqwest::StatusCode::CONFLICT {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(response.body())
        .ok()
        .and_then(|body| body.get("error")?.as_str().map(str::to_owned))
        .is_some_and(|message| message.starts_with("workstream is already active:"))
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
        RunHarnessChoice::Pi => ManagedHarness::Pi,
        RunHarnessChoice::Crush => ManagedHarness::Crush,
        RunHarnessChoice::Omp => ManagedHarness::Omp,
        RunHarnessChoice::Kimi => ManagedHarness::Kimi,
        RunHarnessChoice::CommandCode => ManagedHarness::CommandCode,
        RunHarnessChoice::Kiro => ManagedHarness::Kiro,
        RunHarnessChoice::Grok => ManagedHarness::Grok,
        RunHarnessChoice::Antigravity => ManagedHarness::Antigravity,
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
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::io::Cursor;
    use std::sync::atomic::AtomicUsize;

    use ai_memory_core::{ManagedRunId, WorkstreamId};
    use axum::Router;
    use axum::http::StatusCode;
    use axum::response::IntoResponse as _;
    use axum::routing::post;
    use clap::Parser as _;

    use super::*;
    use crate::cli::{Cli, Command as CliCommand};

    /// `show` filters its harness menu with this, so a false positive would
    /// offer an agent that cannot start.
    #[test]
    fn executable_available_rejects_a_program_that_is_not_installed() {
        assert!(!executable_available(OsStr::new(
            "ai-memory-no-such-harness-binary"
        )));
    }

    /// An absolute path that exists resolves without consulting `PATH`, which
    /// is the branch `--executable` relies on.
    #[test]
    fn executable_available_accepts_an_existing_absolute_path() {
        let current = std::env::current_exe().unwrap();
        assert!(executable_available(current.as_os_str()));
        assert_eq!(
            resolve_program(current.as_os_str()).as_deref(),
            Some(current.as_path())
        );
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
            resolve_candidate(&tmp.path().join("faux-harness")),
            None,
            "an extension-less script is not launchable by CreateProcess"
        );

        std::fs::write(
            tmp.path().join("faux-harness.cmd"),
            "@echo off\r\nexit /b 0\r\n",
        )
        .unwrap();
        let resolved =
            resolve_candidate(&tmp.path().join("faux-harness")).expect("the wrapper resolves");
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
        assert_eq!(resolve_candidate(&current), Some(current));
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
            lease_owner: "workstation:43".into(),
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
        )
        .await
        .unwrap();

        assert_eq!(prepared.workstream_name, "default");
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
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
            automatic_harness_flavor(ManagedHarness::Kiro, ManagedHarness::KiroV3, None),
            ManagedHarness::KiroV3
        );
        assert_eq!(
            automatic_harness_flavor(ManagedHarness::Kiro, ManagedHarness::KiroV3, Some("linked"),),
            ManagedHarness::Kiro
        );
    }

    #[test]
    fn incompatible_link_starts_fresh_in_the_explicit_kiro_engine() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();

        let (fresh, orphaned) = build_preflighted_launch_plan(
            ManagedHarness::KiroV3,
            None,
            vec![OsString::from("--v3")],
            Some("3f6d1c2a-0000-4000-8000-000000000aaa"),
            false,
            temp.path(),
            &cwd,
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

        let (resumed, orphaned) = build_preflighted_launch_plan(
            ManagedHarness::Pi,
            None,
            native_args.clone(),
            Some("linked"),
            false,
            temp.path(),
            &cwd,
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
        )
        .unwrap();
        assert!(orphaned.is_none());
        assert_eq!(explicit.expected_session_id.as_deref(), Some("chosen"));
    }

    #[test]
    fn force_fresh_bypasses_an_existing_link_and_rejects_native_selectors() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let (fresh, orphaned) = build_preflighted_launch_plan(
            ManagedHarness::Claude,
            None,
            Vec::new(),
            Some("linked"),
            true,
            temp.path(),
            &cwd,
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
        )
        .unwrap_err();
        assert!(error.to_string().contains("--fresh cannot be combined"));
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

        let utility = build_launch_plan(
            ManagedHarness::Codex,
            None,
            vec![OsString::from("--version")],
            None,
        )
        .unwrap();
        assert_eq!(utility.mode, LaunchMode::Passthrough);
        assert!(
            resolve_native_session_after_run(
                &utility,
                ManagedHarness::Codex,
                temp.path(),
                &cwd,
                started_at,
                None,
            )
            .await
            .unwrap()
            .is_none()
        );

        let session = build_launch_plan(ManagedHarness::Codex, None, Vec::new(), None).unwrap();
        assert_eq!(
            resolve_native_session_after_run(
                &session,
                ManagedHarness::Codex,
                temp.path(),
                &cwd,
                started_at,
                None,
            )
            .await
            .unwrap()
            .as_deref(),
            Some("unrelated-current")
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
}
