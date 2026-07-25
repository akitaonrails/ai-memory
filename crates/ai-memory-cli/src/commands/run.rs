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
    export_transcript, inspect_repository, list_native_sessions, wait_for_transcript_flush,
};
use anyhow::{Context as _, Result, anyhow};
use tokio::process::Command;

use crate::cli::{RunArgs, RunHarnessChoice};
use crate::commands::{path_util, resolve_project_name};
use crate::config::Config;
use crate::http_client::{
    ServerEndpoint, ServerResponseError, get_json, post_empty, post_json, post_json_no_content,
};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const PREPARE_BUSY_RETRY_WINDOW: Duration = Duration::from_secs(5);
const PREPARE_BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(250);
const IMPORT_BATCH_EVENTS: usize = 400;
const IMPORT_BATCH_BYTES: usize = 1024 * 1024;
const ADOPTION_CANDIDATE_LIMIT: usize = 8;
const AUTO_HARNESSES: [ManagedHarness; 6] = [
    ManagedHarness::Claude,
    ManagedHarness::Codex,
    ManagedHarness::OpenCode,
    ManagedHarness::Pi,
    ManagedHarness::Crush,
    ManagedHarness::Kimi,
];

#[derive(Debug, Clone)]
struct AutoSessionCandidate {
    harness: ManagedHarness,
    session: NativeSessionCandidate,
}

/// Run one native harness and return its exact process exit code.
pub async fn run(config: &Config, args: RunArgs) -> Result<i32> {
    let cwd = std::env::current_dir().context("getting managed run working directory")?;
    let repository = inspect_repository(&cwd)?;
    let home = native_home(config).context("locating native harness session storage")?;
    let automatic_harness = args.harness.is_none();
    let mut native_args = args.native_args;
    let trailing_yolo = remove_wrapper_yolo(&mut native_args);
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
        Some(choice) => managed_harness(choice),
        None => auto_candidates
            .first()
            .map(|candidate| candidate.harness)
            .ok_or_else(no_auto_session_error)?,
    };
    let executable = args.executable.map(PathBuf::into_os_string);
    ensure_executable_available(provisional_harness, executable.as_deref())?;
    let project = resolve_project_name(config, args.project.as_deref())?;
    let may_adopt_native_session = args.new_workstream.is_none();
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let prepare = PrepareManagedRunRequest {
        workspace: args.workspace,
        project,
        cwd: repository.cwd.to_string_lossy().into_owned(),
        repo_fingerprint: repository.repo_fingerprint,
        worktree_fingerprint: repository.worktree_fingerprint,
        agent: provisional_harness.agent_kind(),
        automatic_harness,
        available_agents: auto_candidates
            .iter()
            .map(|candidate| candidate.harness.agent_kind())
            .collect(),
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
    let harness = if automatic_harness {
        let resolved = prepared.resolved_agent.unwrap_or_else(|| {
            eprintln!(
                "ai-memory: the server does not support managed harness precedence; using the newest checkout-local session. Upgrade the server for established-workstream selection"
            );
            provisional_harness.agent_kind()
        });
        acquired_try!(managed_harness_from_agent(resolved).ok_or_else(|| {
            anyhow!(
                "the server selected unsupported automatic harness '{}'",
                resolved.as_str()
            )
        }))
    } else {
        provisional_harness
    };
    acquired_try!(ensure_executable_available(harness, executable.as_deref()));
    let mut plan = acquired_try!(build_launch_plan(
        harness,
        executable.clone(),
        native_args.clone(),
        prepared.native_session_id.as_deref(),
    ));
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
        apply_yolo(harness, &mut plan.args);
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
    if interrupted_before_spawn.load(Ordering::SeqCst) {
        acquired_try!(Err(anyhow!(
            "managed run interrupted before the agent started"
        )));
    }

    let started_at = SystemTime::now();
    let mut command = Command::new(&plan.program);
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
    if harness == ManagedHarness::Crush
        && let Err(error) = post_empty_with_retry(
            &endpoint,
            &format!("{run_path}/context/accept"),
            "acknowledging Crush managed context",
        )
        .await
    {
        eprintln!("ai-memory: {error}; the context may be delivered again on the next Crush run");
    }

    let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    heartbeat.tick().await;
    let status = loop {
        tokio::select! {
            result = child.wait() => break acquired_try!(result.context("waiting for managed harness")),
            _ = heartbeat.tick() => {
                if let Err(error) = post_empty(&endpoint, &format!("{run_path}/heartbeat")).await {
                    eprintln!("ai-memory: managed workstream heartbeat failed: {error}");
                }
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
        "no Claude Code, Codex, OpenCode, Pi, Crush, or Kimi Code session was found for this directory; start one explicitly with `ai-memory run claude`, `ai-memory run codex`, `ai-memory run opencode`, `ai-memory run pi`, `ai-memory run crush`, or `ai-memory run kimi`"
    )
}

fn remove_wrapper_yolo(args: &mut Vec<OsString>) -> bool {
    let before = args.len();
    args.retain(|arg| arg != OsStr::new("--yolo"));
    args.len() != before
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

fn executable_available(program: &OsStr) -> bool {
    let path = Path::new(program);
    if path.components().count() > 1 {
        return executable_path_available(path);
    }
    std::env::var_os("PATH").is_some_and(|path_value| {
        std::env::split_paths(&path_value)
            .map(|dir| dir.join(path))
            .any(|candidate| executable_path_available(&candidate))
    })
}

fn executable_path_available(path: &Path) -> bool {
    if executable_file(path) {
        return true;
    }
    #[cfg(windows)]
    if path.extension().is_none() {
        let extensions =
            std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        return extensions
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(|extension| path.with_extension(extension.trim_start_matches('.')))
            .any(|candidate| executable_file(&candidate));
    }
    false
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
    let selection = loop {
        tokio::select! {
            result = &mut chooser => {
                break result.context("waiting for the native session choice")?;
            }
            _ = heartbeat.tick() => {
                if let Err(error) = post_empty(endpoint, &format!("{run_path}/heartbeat")).await {
                    eprintln!("ai-memory: managed workstream heartbeat failed: {error}");
                }
            }
        }
    };
    post_empty(endpoint, &format!("{run_path}/heartbeat"))
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

fn session_age(updated_at: SystemTime, now: SystemTime) -> String {
    let age = now.duration_since(updated_at).unwrap_or_default().as_secs();
    match age {
        0..60 => "just now".into(),
        60..3_600 => plural_age(age / 60, "minute"),
        3_600..86_400 => plural_age(age / 3_600, "hour"),
        _ => plural_age(age / 86_400, "day"),
    }
}

fn plural_age(value: u64, unit: &str) -> String {
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
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
    fn lease_owner_uses_the_resolved_host_and_process() {
        assert_eq!(lease_owner_label(Some("workstation"), 42), "workstation:42");
        assert_eq!(lease_owner_label(None, 42), "localhost:42");
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
            Duration::from_millis(100),
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
