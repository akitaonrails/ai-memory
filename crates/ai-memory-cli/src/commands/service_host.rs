//! `ai-memory service-host`: the background host behind the Windows logon
//! entry `ai-memory setup` installs.
//!
//! Windows has no per-user service manager like systemd `--user` or launchd.
//! A real Windows service needs an administrator to install it and, because
//! the data dir lives in the user's profile, the account password to run it
//! (docs/windows.md, Scenario E). A Scheduled Task runs its action inside a
//! job object that kills the server's process tree when the action exits, and
//! shows a console window while it runs. So `setup` writes an `ai-memory`
//! value under the per-user `HKCU\...\CurrentVersion\Run` key instead, which
//! needs no elevation, and that value runs this command at every logon:
//!
//! 1. Without `--supervise`, `service-host` starts itself again detached from
//!    any console (`DETACHED_PROCESS | CREATE_NO_WINDOW`, breaking away from a
//!    job when allowed) and exits, so the logon console closes at once.
//! 2. With `--supervise`, it runs `serve --transport http --enable-web` on the
//!    same data dir and config with a hidden console (`CREATE_NO_WINDOW`), so
//!    neither the server nor a process it starts (the Git CLI fallback, a
//!    provider CLI) opens a window. It waits on the server and restarts it
//!    after a failure, the way `Restart=on-failure` and launchd's `KeepAlive`
//!    do, until the server exits cleanly or keeps failing within seconds of
//!    starting (another server on the port, a broken config).
//!
//! The supervisor records its own pid and the server's in
//! `<data_dir>/service-host.json`, which `setup` reads to tell whether the
//! host runs and `uninstall` reads to stop it.
//!
//! Stopping is cooperative: [`stop`] writes `<data_dir>/service-host.stop`,
//! and the supervisor, which polls for it, kills the server through the
//! `Child` it owns and exits. Endpoint protection counts an unsigned program
//! running `taskkill` against a process it found by pid as one more malicious
//! signal (#1280), and a handle the supervisor holds cannot reach a process
//! that reused a recorded pid. Only a supervisor that does not answer within
//! [`STOP_TIMEOUT`] (one started by an older binary, or a hung one), or a
//! server whose supervisor already died, still gets `taskkill`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use super::user_service::CommandRunner;
use crate::cli::ServiceHostArgs;
use crate::config::Config;

/// The host's state file name, under the data dir.
const STATE_FILE: &str = "service-host.json";
/// The extension that turns the state file's name into the stop request's.
const STOP_REQUEST_EXTENSION: &str = "stop";

/// How long [`stop`] waits for the supervisor to answer a stop request before
/// it falls back to `taskkill`.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the supervisor looks for a stop request, and [`stop`] for the
/// host to be gone.
const STOP_POLL: Duration = Duration::from_millis(250);

/// Wait before restarting a server that failed.
const RESTART_DELAY: Duration = Duration::from_secs(5);
/// A server that fails sooner than this after starting counts as a quick
/// failure; [`MAX_QUICK_FAILURES`] in a row stop the host.
const QUICK_FAILURE: Duration = Duration::from_secs(10);
const MAX_QUICK_FAILURES: u32 = 5;

/// What the running host recorded about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct HostState {
    pub(crate) supervisor_pid: u32,
    pub(crate) server_pid: Option<u32>,
}

/// Where the host keeps its state for this data dir.
pub(crate) fn state_file(data_dir: &Path) -> PathBuf {
    data_dir.join(STATE_FILE)
}

/// The file whose presence asks the host behind `state_file` to stop.
fn stop_request_file(state_file: &Path) -> PathBuf {
    state_file.with_extension(STOP_REQUEST_EXTENSION)
}

fn read_state(path: &Path) -> Option<HostState> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_state(path: &Path, state: HostState) -> Result<()> {
    let text = serde_json::to_string(&state)?;
    ai_memory_wiki::write_atomic(path, text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// When `pid` is a live ai-memory process, its start time. A recorded pid the
/// system has since handed to another program does not count, and the start
/// time tells a later ai-memory process on the same pid apart.
fn ai_memory_start_time(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::new(),
    );
    system
        .process(pid)
        .filter(|process| {
            process
                .name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("ai-memory")
        })
        .map(sysinfo::Process::start_time)
}

/// Whether `pid` is a live ai-memory process.
fn is_live_ai_memory(pid: u32) -> bool {
    ai_memory_start_time(pid).is_some()
}

/// The recorded host when its supervisor is still alive.
pub(crate) fn running(state_file: &Path) -> Option<HostState> {
    read_state(state_file).filter(|state| is_live_ai_memory(state.supervisor_pid))
}

/// Every live pid the host recorded, supervisor first; `uninstall
/// --purge-data` leaves them out of its live-process check because it is
/// about to stop them.
pub(crate) fn live_pids(state_file: &Path) -> Vec<u32> {
    read_state(state_file)
        .map(|state| {
            std::iter::once(state.supervisor_pid)
                .chain(state.server_pid)
                .filter(|pid| is_live_ai_memory(*pid))
                .collect()
        })
        .unwrap_or_default()
}

/// Stop a running host. A host that is not running is no error. The state
/// file and the stop request go with it.
///
/// # Errors
/// When the stop request cannot be written, or when the supervisor did not
/// answer it in time and `taskkill` could not be run or refused a process
/// that is still alive afterwards.
pub(crate) fn stop(state_file: &Path, runner: &dyn CommandRunner) -> Result<()> {
    stop_with(
        state_file,
        runner,
        &ai_memory_start_time,
        STOP_TIMEOUT,
        STOP_POLL,
    )
}

/// [`stop`] with the process probe and the wait injected. `start_time` gives
/// a pid's start time while it is a live ai-memory process.
///
/// A live supervisor gets the stop request and `timeout` to exit with its
/// server. A recorded process that is still the same one afterwards (the
/// supervisor ignored the request, or had died and left its server behind)
/// gets `taskkill`, the supervisor's tree first.
fn stop_with(
    state_file: &Path,
    runner: &dyn CommandRunner,
    start_time: &dyn Fn(u32) -> Option<u64>,
    timeout: Duration,
    poll: Duration,
) -> Result<()> {
    let request = stop_request_file(state_file);
    if let Some(state) = read_state(state_file) {
        let track = |pid: u32| start_time(pid).map(|started| (pid, started));
        let supervisor = track(state.supervisor_pid);
        let tracked: Vec<(u32, u64)> = supervisor
            .into_iter()
            .chain(state.server_pid.and_then(track))
            .collect();
        let still_running = |&(pid, started): &(u32, u64)| start_time(pid) == Some(started);
        if supervisor.is_some() {
            ai_memory_wiki::write_atomic(&request, b"")
                .with_context(|| format!("writing {}", request.display()))?;
            let deadline = Instant::now() + timeout;
            while tracked.iter().any(still_running) && Instant::now() < deadline {
                std::thread::sleep(poll);
            }
        }
        for target in tracked.iter().filter(|target| still_running(target)) {
            let pid_text = target.0.to_string();
            let outcome = runner.run("taskkill", &["/PID", &pid_text, "/T", "/F"])?;
            if !outcome.success && still_running(target) {
                anyhow::bail!(
                    "`taskkill /PID {pid_text} /T /F` failed: {}",
                    outcome.stderr.trim()
                );
            }
        }
    }
    remove_if_present(&request)?;
    remove_if_present(state_file)
}

fn remove_if_present(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("removing {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// What the supervisor does after the server exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    Stop,
    RestartAfter(Duration),
}

/// Decide after one server run. A clean exit is deliberate and ends the host;
/// a failure restarts it, unless the server has now failed quickly
/// [`MAX_QUICK_FAILURES`] times in a row, which a restart would not fix.
fn after_exit(succeeded: bool, ran: Duration, quick_failures: &mut u32) -> Next {
    if succeeded {
        return Next::Stop;
    }
    if ran < QUICK_FAILURE {
        *quick_failures += 1;
    } else {
        *quick_failures = 1;
    }
    if *quick_failures >= MAX_QUICK_FAILURES {
        Next::Stop
    } else {
        Next::RestartAfter(RESTART_DELAY)
    }
}

/// Run the server again and again per [`after_exit`]. `run_server` starts one
/// server, reports its pid through `started`, waits for it (killing it when
/// a stop is requested), and returns whether it exited successfully. A stop
/// request ends the loop as a clean exit, whatever the server's exit status.
/// Returns the last outcome.
fn supervise(
    mut run_server: impl FnMut(&mut dyn FnMut(u32)) -> Result<bool>,
    mut started: impl FnMut(u32),
    mut sleep: impl FnMut(Duration),
    clock: impl Fn() -> Instant,
    stop_requested: impl Fn() -> bool,
) -> Result<bool> {
    let mut quick_failures = 0;
    loop {
        if stop_requested() {
            return Ok(true);
        }
        let began = clock();
        let succeeded = run_server(&mut started)?;
        if stop_requested() {
            return Ok(true);
        }
        match after_exit(succeeded, clock() - began, &mut quick_failures) {
            Next::Stop => return Ok(succeeded),
            Next::RestartAfter(delay) => sleep(delay),
        }
    }
}

/// Sleep for `delay`, but return as soon as a stop is requested, so a stop
/// that lands between two server runs does not wait out the restart delay.
fn sleep_unless_stopped(delay: Duration, stop_requested: &dyn Fn() -> bool) {
    let deadline = Instant::now() + delay;
    while !stop_requested() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        std::thread::sleep(left.min(STOP_POLL));
    }
}

/// The global arguments that point a child at this install. Paths are passed
/// in their plain spelling, the one the logon entry uses: the data dir is
/// canonical, and its `\\?\` prefix otherwise reaches the command lines of
/// the supervisor and the server, where an EDR trace showed it (#1280).
pub(crate) fn install_args(config: &Config, config_path: Option<&Path>) -> Vec<std::ffi::OsString> {
    let mut args = vec!["--data-dir".into(), plain_arg(&config.data_dir)];
    if let Some(path) = config_path {
        args.push("--config".into());
        args.push(plain_arg(path));
    }
    args
}

/// `path` without a `\\?\` verbatim prefix. A path that is not UTF-8 is
/// passed as it is.
fn plain_arg(path: &Path) -> std::ffi::OsString {
    match path.to_str() {
        Some(text) => super::path_util::strip_windows_verbatim_prefix(text)
            .into_owned()
            .into(),
        None => path.as_os_str().to_owned(),
    }
}

/// Run `service-host`.
///
/// # Errors
/// On a platform other than Windows, or when the host cannot start itself or
/// the server.
pub async fn run(config: &Config, args: ServiceHostArgs, config_path: Option<&Path>) -> Result<()> {
    let i18n = &config.i18n;
    if !cfg!(windows) {
        anyhow::bail!(
            "{}", i18n.text("cli-service-host-service-host-is-the-windows-logon-entry-s-launcher-on-this-platform-ai-memor"));
    }
    // `current_exe()` can return a `\\?\` verbatim path; the host launches
    // itself and the server with it, so strip the prefix there too (#1280).
    let exe = std::env::current_exe()
        .context(i18n.text("cli-service-host-locating-the-running-ai-memory-binary"))?;
    let exe = std::path::PathBuf::from(plain_arg(&exe));
    let install = install_args(config, config_path);
    if !args.supervise {
        let mut host_args = install.clone();
        host_args.extend(["service-host".into(), "--supervise".into()]);
        return windows::spawn_detached(&exe, &host_args)
            .with_context(|| format!("starting {} service-host --supervise", exe.display()));
    }

    let state_path = state_file(&config.data_dir);
    let own_pid = std::process::id();
    if running(&state_path).is_some_and(|state| state.supervisor_pid != own_pid) {
        // A second logon, or `setup` racing the Run entry: one host serves.
        return Ok(());
    }
    // A request left behind by a stop that never finished must not end this
    // host as soon as it starts.
    let stop_request = stop_request_file(&state_path);
    remove_if_present(&stop_request)?;
    write_state(
        &state_path,
        HostState {
            supervisor_pid: own_pid,
            server_pid: None,
        },
    )?;
    let mut server_args = install;
    server_args.extend(["serve", "--transport", "http", "--enable-web"].map(Into::into));
    let outcome = tokio::task::spawn_blocking(move || {
        let stop_requested = || stop_request.exists();
        supervise(
            |started| windows::run_hidden(&exe, &server_args, started, &stop_requested),
            |pid| {
                let state = HostState {
                    supervisor_pid: own_pid,
                    server_pid: Some(pid),
                };
                if let Err(error) = write_state(&state_path, state) {
                    tracing::warn!(error = %format!("{error:#}"), "service-host: recording the server pid");
                }
            },
            |delay| sleep_unless_stopped(delay, &stop_requested),
            Instant::now,
            stop_requested,
        )
        .map(|succeeded| (succeeded, state_path))
    })
    .await
    .context(i18n.text("cli-service-host-the-service-host-supervisor-stopped-unexpectedly"))?;
    let (succeeded, state_path) = outcome?;
    std::fs::remove_file(&state_path).ok();
    if !succeeded {
        tracing::error!(
            "service-host: the server failed {MAX_QUICK_FAILURES} times within {}s of starting; \
             not restarting it. Run `ai-memory serve --transport http` in a terminal to see why.",
            QUICK_FAILURE.as_secs()
        );
    }
    Ok(())
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsString;
    use std::os::windows::process::CommandExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use anyhow::{Context as _, Result};

    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;

    fn command(exe: &Path, args: &[OsString], flags: u32) -> Command {
        let mut command = Command::new(exe);
        command
            .args(args)
            .creation_flags(flags)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// Start the supervisor with no console, out of whatever job started the
    /// logon entry when the job allows it, and do not wait for it.
    pub(super) fn spawn_detached(exe: &Path, args: &[OsString]) -> Result<()> {
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
        match command(exe, args, flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
            Ok(_) => Ok(()),
            Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
                command(exe, args, flags)
                    .spawn()
                    .map(|_| ())
                    .map_err(Into::into)
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Run the server on a hidden console of its own, which the processes it
    /// starts inherit instead of opening windows, and wait for it. When a stop
    /// is requested, kill it through the handle this process owns, which no
    /// other process can have taken over the way a pid can be reused.
    pub(super) fn run_hidden(
        exe: &Path,
        args: &[OsString],
        started: &mut dyn FnMut(u32),
        stop_requested: &dyn Fn() -> bool,
    ) -> Result<bool> {
        let mut child = command(exe, args, CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
            .spawn()
            .with_context(|| format!("starting {} serve", exe.display()))?;
        started(child.id());
        loop {
            if let Some(status) = child.try_wait().context("waiting for the server")? {
                return Ok(status.success());
            }
            if stop_requested() {
                child.kill().context("stopping the server")?;
                return Ok(child.wait().context("waiting for the server")?.success());
            }
            std::thread::sleep(super::STOP_POLL);
        }
    }
}

#[cfg(not(windows))]
mod windows {
    use std::ffi::OsString;
    use std::path::Path;

    use anyhow::{Result, bail};

    pub(super) fn spawn_detached(_exe: &Path, _args: &[OsString]) -> Result<()> {
        bail!("service-host runs only on Windows")
    }

    pub(super) fn run_hidden(
        _exe: &Path,
        _args: &[OsString],
        _started: &mut dyn FnMut(u32),
        _stop_requested: &dyn Fn() -> bool,
    ) -> Result<bool> {
        bail!("service-host runs only on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    /// A clean exit stops the host; failures restart it after the delay; a
    /// server that keeps failing quickly stops it on the fifth, and one that
    /// ran a while before failing starts the count over.
    #[test]
    fn restart_policy_follows_exit_status_and_how_long_it_ran() {
        let mut quick = 0;
        assert_eq!(after_exit(true, Duration::ZERO, &mut quick), Next::Stop);
        for _ in 1..MAX_QUICK_FAILURES {
            assert_eq!(
                after_exit(false, Duration::from_secs(1), &mut quick),
                Next::RestartAfter(RESTART_DELAY)
            );
        }
        assert_eq!(
            after_exit(false, Duration::from_secs(1), &mut quick),
            Next::Stop
        );

        let mut quick = MAX_QUICK_FAILURES - 1;
        assert_eq!(
            after_exit(false, Duration::from_secs(3600), &mut quick),
            Next::RestartAfter(RESTART_DELAY)
        );
        assert_eq!(quick, 1);
    }

    /// The loop restarts a failing server, reports each pid, sleeps between
    /// runs, and stops once the server exits cleanly.
    #[test]
    fn supervise_restarts_until_a_clean_exit() {
        let outcomes = RefCell::new(vec![false, false, true].into_iter());
        let pids = RefCell::new(Vec::new());
        let sleeps = Cell::new(0);
        let next_pid = Cell::new(100);
        let succeeded = supervise(
            |started| {
                next_pid.set(next_pid.get() + 1);
                started(next_pid.get());
                Ok(outcomes.borrow_mut().next().unwrap())
            },
            |pid| pids.borrow_mut().push(pid),
            |delay| {
                assert_eq!(delay, RESTART_DELAY);
                sleeps.set(sleeps.get() + 1);
            },
            Instant::now,
            || false,
        )
        .unwrap();
        assert!(succeeded);
        assert_eq!(*pids.borrow(), vec![101, 102, 103]);
        assert_eq!(sleeps.get(), 2);
    }

    /// A server that never stays up stops the loop instead of spinning.
    #[test]
    fn supervise_gives_up_on_a_server_that_keeps_failing() {
        let runs = Cell::new(0);
        let succeeded = supervise(
            |_| {
                runs.set(runs.get() + 1);
                Ok(false)
            },
            |_| {},
            |_| {},
            Instant::now,
            || false,
        )
        .unwrap();
        assert!(!succeeded);
        assert_eq!(runs.get(), MAX_QUICK_FAILURES);
    }

    /// A server killed for a stop request fails, but the loop ends as a clean
    /// exit instead of restarting it; a request already there runs nothing.
    #[test]
    fn supervise_ends_cleanly_on_a_stop_request() {
        let requested = Cell::new(false);
        let runs = Cell::new(0);
        let succeeded = supervise(
            |_| {
                runs.set(runs.get() + 1);
                requested.set(true);
                Ok(false)
            },
            |_| {},
            |_| panic!("a stopped host must not wait to restart"),
            Instant::now,
            || requested.get(),
        )
        .unwrap();
        assert!(succeeded);
        assert_eq!(runs.get(), 1);

        let succeeded = supervise(
            |_| panic!("a host asked to stop must not start the server"),
            |_| {},
            |_| {},
            Instant::now,
            || true,
        )
        .unwrap();
        assert!(succeeded);
    }

    #[test]
    fn the_restart_delay_ends_early_on_a_stop_request() {
        let began = Instant::now();
        sleep_unless_stopped(Duration::from_secs(60), &|| true);
        assert!(began.elapsed() < Duration::from_secs(5));
    }

    /// Records every command; `taskkill` on a pid marks it, and its tree,
    /// dead in the fake process table.
    struct Taskkill<'a> {
        calls: &'a RefCell<Vec<String>>,
        dead: &'a RefCell<Vec<u32>>,
        tree: &'a [u32],
    }
    impl CommandRunner for Taskkill<'_> {
        fn run(
            &self,
            program: &str,
            args: &[&str],
        ) -> Result<super::super::user_service::CommandOutcome> {
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            if program == "taskkill" {
                let pid: u32 = args[1].parse().unwrap();
                let mut dead = self.dead.borrow_mut();
                dead.push(pid);
                if self.tree.first() == Some(&pid) {
                    dead.extend_from_slice(self.tree);
                }
            }
            Ok(super::super::user_service::CommandOutcome {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        }
    }

    const SUPERVISOR: u32 = 4100;
    const SERVER: u32 = 4200;

    fn recorded_host(dir: &Path) -> PathBuf {
        let path = state_file(dir);
        write_state(
            &path,
            HostState {
                supervisor_pid: SUPERVISOR,
                server_pid: Some(SERVER),
            },
        )
        .unwrap();
        path
    }

    /// A supervisor that answers the stop request takes its server with it,
    /// and `stop` runs no `taskkill` at all (#1280).
    #[test]
    fn stop_asks_a_live_supervisor_and_runs_no_taskkill() {
        let tmp = tempfile::tempdir().unwrap();
        let path = recorded_host(tmp.path());
        let request = stop_request_file(&path);
        let calls = RefCell::new(Vec::new());
        let dead = RefCell::new(Vec::new());
        let runner = Taskkill {
            calls: &calls,
            dead: &dead,
            tree: &[],
        };
        let saw_request = Cell::new(false);
        let probe = |pid: u32| {
            if request.exists() {
                saw_request.set(true);
                return None;
            }
            Some(u64::from(pid))
        };
        stop_with(
            &path,
            &runner,
            &probe,
            Duration::from_secs(60),
            Duration::from_millis(1),
        )
        .unwrap();
        assert!(saw_request.get(), "the supervisor was never asked to stop");
        assert!(calls.borrow().is_empty(), "{:?}", calls.borrow());
        assert!(!path.exists());
        assert!(!request.exists());
    }

    /// A supervisor that ignores the request (an older binary, a hung one)
    /// gets `taskkill` on its tree once the timeout runs out.
    #[test]
    fn stop_falls_back_to_taskkill_when_the_supervisor_does_not_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = recorded_host(tmp.path());
        let calls = RefCell::new(Vec::new());
        let dead = RefCell::new(Vec::new());
        let runner = Taskkill {
            calls: &calls,
            dead: &dead,
            tree: &[SUPERVISOR, SERVER],
        };
        let probe = |pid: u32| (!dead.borrow().contains(&pid)).then_some(u64::from(pid));
        stop_with(
            &path,
            &runner,
            &probe,
            Duration::from_millis(20),
            Duration::from_millis(1),
        )
        .unwrap();
        assert_eq!(
            *calls.borrow(),
            vec![format!("taskkill /PID {SUPERVISOR} /T /F")]
        );
        assert!(!path.exists());
        assert!(!stop_request_file(&path).exists());
    }

    /// A server left behind by a supervisor that already died gets
    /// `taskkill` at once: nobody is there to answer a request.
    #[test]
    fn stop_kills_an_orphaned_server_without_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let path = recorded_host(tmp.path());
        let request = stop_request_file(&path);
        let calls = RefCell::new(Vec::new());
        let dead = RefCell::new(vec![SUPERVISOR]);
        let runner = Taskkill {
            calls: &calls,
            dead: &dead,
            tree: &[],
        };
        let probe = |pid: u32| {
            assert!(!request.exists(), "no request for a dead supervisor");
            (!dead.borrow().contains(&pid)).then_some(u64::from(pid))
        };
        let began = Instant::now();
        stop_with(
            &path,
            &runner,
            &probe,
            Duration::from_secs(60),
            Duration::from_millis(1),
        )
        .unwrap();
        assert!(began.elapsed() < Duration::from_secs(5));
        assert_eq!(
            *calls.borrow(),
            vec![format!("taskkill /PID {SERVER} /T /F")]
        );
    }

    /// A recorded pid now held by another ai-memory process (same pid, other
    /// start time) is not the host's: the fallback leaves it alone.
    #[test]
    fn stop_never_kills_a_process_that_reused_a_recorded_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let path = recorded_host(tmp.path());
        let calls = RefCell::new(Vec::new());
        let dead = RefCell::new(Vec::new());
        let runner = Taskkill {
            calls: &calls,
            dead: &dead,
            tree: &[],
        };
        let probes = Cell::new(0_u64);
        let probe = |pid: u32| {
            probes.set(probes.get() + 1);
            // The first look records the host; every later one finds the
            // pids alive again, but started by someone else.
            Some(u64::from(pid) + u64::from(probes.get() > 2))
        };
        stop_with(
            &path,
            &runner,
            &probe,
            Duration::from_millis(20),
            Duration::from_millis(1),
        )
        .unwrap();
        assert!(calls.borrow().is_empty(), "{:?}", calls.borrow());
    }

    /// A state file naming no live ai-memory process means no host runs, and
    /// stopping it then runs nothing and still removes the stale file.
    #[test]
    fn a_stale_state_file_is_not_a_running_host() {
        struct NoCalls;
        impl CommandRunner for NoCalls {
            fn run(
                &self,
                program: &str,
                _args: &[&str],
            ) -> Result<super::super::user_service::CommandOutcome> {
                panic!("{program} must not run for a dead host");
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = state_file(tmp.path());
        assert_eq!(running(&path), None);
        write_state(
            &path,
            HostState {
                supervisor_pid: u32::MAX - 1,
                server_pid: Some(u32::MAX - 2),
            },
        )
        .unwrap();
        assert_eq!(running(&path), None);
        assert!(live_pids(&path).is_empty());
        stop(&path, &NoCalls).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn children_are_pointed_at_the_same_install() {
        let config = Config {
            data_dir: PathBuf::from("/data"),
            ..Config::default()
        };
        assert_eq!(
            install_args(&config, Some(Path::new("/data/config.toml"))),
            ["--data-dir", "/data", "--config", "/data/config.toml"]
                .map(std::ffi::OsString::from)
                .to_vec()
        );
        assert_eq!(
            install_args(&config, None),
            ["--data-dir", "/data"]
                .map(std::ffi::OsString::from)
                .to_vec()
        );
    }

    /// The data dir is canonical, so on Windows it carries a `\\?\` prefix;
    /// the supervisor and the server get the plain spelling (#1280).
    #[test]
    fn children_get_plain_paths() {
        let config = Config {
            data_dir: PathBuf::from(r"\\?\C:\Users\me\AppData\Local\ai-memory"),
            ..Config::default()
        };
        let config_path = Path::new(r"\\?\C:\Users\me\AppData\Local\ai-memory\config.toml");
        assert_eq!(
            install_args(&config, Some(config_path)),
            [
                "--data-dir",
                r"C:\Users\me\AppData\Local\ai-memory",
                "--config",
                r"C:\Users\me\AppData\Local\ai-memory\config.toml",
            ]
            .map(std::ffi::OsString::from)
            .to_vec()
        );

        // A data dir on a share keeps its UNC spelling, and spaces survive.
        let config = Config {
            data_dir: PathBuf::from(r"\\?\UNC\server\share\ai memory"),
            ..Config::default()
        };
        assert_eq!(
            install_args(&config, None),
            ["--data-dir", r"\\server\share\ai memory"]
                .map(std::ffi::OsString::from)
                .to_vec()
        );
    }

    #[test]
    fn service_host_refuses_off_windows() {
        if cfg!(windows) {
            return;
        }
        let config = Config::default();
        let error = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(run(&config, ServiceHostArgs { supervise: false }, None))
            .unwrap_err()
            .to_string();
        assert!(error.contains("Windows logon entry"), "{error}");
    }
}
