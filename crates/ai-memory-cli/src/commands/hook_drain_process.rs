//! Detached native process launcher for background hook-spool drains.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Rotate `hook-drain.log` once it grows past this. The log only receives
/// drainer warnings, but a chronically unreachable server emits one on every
/// session boundary forever — without a cap the append-only file grows
/// unbounded. One rotation generation (`.old`) keeps the recent history.
const MAX_DRAIN_LOG_BYTES: u64 = 1024 * 1024;

/// Rename an oversized log to `<name>.old` (replacing any previous `.old`)
/// so the next append starts a fresh file. Best-effort: a failed rotation
/// must never block spawning the drainer.
fn rotate_oversized_log(log: &Path, max_bytes: u64) {
    let Ok(meta) = std::fs::metadata(log) else {
        return;
    };
    if meta.len() <= max_bytes {
        return;
    }
    let mut old = log.as_os_str().to_os_string();
    old.push(".old");
    let _ = std::fs::rename(log, PathBuf::from(old));
}

/// Unit-testable description of the hidden drainer command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainCommandSpec {
    /// Executable path.
    pub exe: PathBuf,
    /// Command arguments.
    pub args: Vec<OsString>,
    /// Stderr log file under `<data_dir>/logs`.
    pub stderr_log: PathBuf,
    /// Current static bearer to hand the child in its environment, when the
    /// spawning hook has one.
    ///
    /// The environment rather than an argument: a flag would put the
    /// credential in the process table for the lifetime of the drain, readable
    /// by anyone who can run `ps`.
    pub live_token: Option<OsString>,
}

/// Unit-testable stdio/detach configuration for a drainer process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnConfig {
    /// stdin is redirected to null.
    pub stdin_null: bool,
    /// stdout is redirected to null.
    pub stdout_null: bool,
    /// stderr is redirected to this file.
    pub stderr_file: PathBuf,
    /// Platform detach flags applied to the command.
    pub detach: DetachConfig,
}

/// Platform detach configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetachConfig {
    /// Unix starts the drainer in a new session through a trusted `setsid`
    /// launcher when available; otherwise it falls back to a new process group.
    #[cfg(unix)]
    UnixNewSession { setsid: Option<PathBuf> },
    /// Windows creation flags.
    #[cfg(windows)]
    WindowsCreationFlags(u32),
    /// No extra detach flags on unsupported platforms.
    #[cfg(not(any(unix, windows)))]
    None,
}

/// Build `ai-memory --data-dir <dir> hook-drain`.
///
/// `live_token` is the bearer the spawning hook is currently configured with.
/// A spooled envelope froze its own credential at capture time, so handing the
/// drain a current one is what lets it recover entries stranded by a rotation
/// (#542).
pub fn command_spec(data_dir: &Path, live_token: Option<&str>) -> io::Result<DrainCommandSpec> {
    Ok(DrainCommandSpec {
        exe: std::env::current_exe()?,
        args: vec![
            OsString::from("--data-dir"),
            data_dir.as_os_str().to_os_string(),
            OsString::from("hook-drain"),
        ],
        stderr_log: data_dir.join("logs").join("hook-drain.log"),
        live_token: live_token.filter(|t| !t.is_empty()).map(OsString::from),
    })
}

/// Build the stdio/detach shape used when spawning.
#[must_use]
pub fn spawn_config(spec: &DrainCommandSpec, try_breakaway: bool) -> SpawnConfig {
    SpawnConfig {
        stdin_null: true,
        stdout_null: true,
        stderr_file: spec.stderr_log.clone(),
        detach: detach_config(try_breakaway),
    }
}

/// Spawn the hidden drainer without inheriting hook stdio.
pub fn spawn(data_dir: &Path, live_token: Option<&str>) -> io::Result<()> {
    let spec = command_spec(data_dir, live_token)?;
    spawn_spec(&spec)
}

/// Hand the child the current bearer in its environment, when there is one.
/// Applied to every spawn path so the breakaway-retry cannot silently drop it.
fn apply_live_token(command: &mut Command, spec: &DrainCommandSpec) {
    if let Some(token) = &spec.live_token {
        command.env(crate::commands::hook_spool::LIVE_TOKEN_ENV, token);
    }
}

fn spawn_spec(spec: &DrainCommandSpec) -> io::Result<()> {
    if let Some(parent) = spec.stderr_log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    rotate_oversized_log(&spec.stderr_log, MAX_DRAIN_LOG_BYTES);
    let config = spawn_config(spec, true);
    let stderr = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.stderr_file)?;

    let mut command = command_for_spec(spec, &config.detach);
    apply_live_token(&mut command, spec);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr));

    match command.spawn() {
        Ok(_child) => Ok(()),
        Err(err) if should_retry_without_breakaway(&err) => {
            let mut log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&spec.stderr_log)?;
            let _ = writeln!(
                log,
                "ai-memory hook-drain: breakaway spawn failed with access denied; retrying without breakaway"
            );
            let config = spawn_config(spec, false);
            let stderr = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&config.stderr_file)?;
            let mut retry = command_for_spec(spec, &config.detach);
            apply_live_token(&mut retry, spec);
            retry
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(stderr));
            retry.spawn().map(|_| ())
        }
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn detach_config(_try_breakaway: bool) -> DetachConfig {
    DetachConfig::UnixNewSession {
        setsid: trusted_setsid_launcher(),
    }
}

#[cfg(unix)]
fn command_for_spec(spec: &DrainCommandSpec, detach: &DetachConfig) -> Command {
    use std::os::unix::process::CommandExt as _;

    match detach {
        DetachConfig::UnixNewSession {
            setsid: Some(setsid),
        } => {
            let mut command = Command::new(setsid);
            command.arg(&spec.exe).args(&spec.args);
            command
        }
        DetachConfig::UnixNewSession { setsid: None } => {
            let mut command = Command::new(&spec.exe);
            command.args(&spec.args);
            command.process_group(0);
            command
        }
    }
}

#[cfg(unix)]
fn trusted_setsid_launcher() -> Option<PathBuf> {
    [Path::new("/usr/bin/setsid"), Path::new("/bin/setsid")]
        .into_iter()
        .find(|path| path.is_file())
        .map(Path::to_path_buf)
}

#[cfg(windows)]
fn detach_config(try_breakaway: bool) -> DetachConfig {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;

    let mut flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW;
    if try_breakaway {
        flags |= CREATE_BREAKAWAY_FROM_JOB;
    }
    DetachConfig::WindowsCreationFlags(flags)
}

#[cfg(windows)]
fn apply_detach_flags(command: &mut Command, detach: &DetachConfig) {
    use std::os::windows::process::CommandExt as _;
    let DetachConfig::WindowsCreationFlags(flags) = detach;
    command.creation_flags(*flags);
}

#[cfg(windows)]
fn command_for_spec(spec: &DrainCommandSpec, detach: &DetachConfig) -> Command {
    let mut command = Command::new(&spec.exe);
    command.args(&spec.args);
    apply_detach_flags(&mut command, detach);
    command
}

#[cfg(not(any(unix, windows)))]
fn detach_config(_try_breakaway: bool) -> DetachConfig {
    DetachConfig::None
}

#[cfg(not(any(unix, windows)))]
fn command_for_spec(spec: &DrainCommandSpec, _detach: &DetachConfig) -> Command {
    let mut command = Command::new(&spec.exe);
    command.args(&spec.args);
    command
}

#[cfg(not(windows))]
fn should_retry_without_breakaway(_err: &io::Error) -> bool {
    false
}

#[cfg(windows)]
fn should_retry_without_breakaway(err: &io::Error) -> bool {
    err.raw_os_error() == Some(5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_spec_uses_data_dir_then_hidden_subcommand() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = command_spec(tmp.path(), None).unwrap();

        assert_eq!(
            spec.args,
            vec![
                OsString::from("--data-dir"),
                tmp.path().as_os_str().to_os_string(),
                OsString::from("hook-drain"),
            ]
        );
        assert_eq!(
            spec.stderr_log,
            tmp.path().join("logs").join("hook-drain.log")
        );
    }

    #[test]
    fn rotate_oversized_log_rotates_once_over_cap_and_keeps_small_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("hook-drain.log");
        let old = tmp.path().join("hook-drain.log.old");

        // Under the cap: untouched.
        std::fs::write(&log, b"small").unwrap();
        rotate_oversized_log(&log, 16);
        assert!(log.exists());
        assert!(!old.exists());

        // Over the cap: current becomes .old, next append starts fresh.
        std::fs::write(&log, vec![b'x'; 32]).unwrap();
        rotate_oversized_log(&log, 16);
        assert!(!log.exists());
        assert_eq!(std::fs::metadata(&old).unwrap().len(), 32);

        // A second rotation replaces the previous .old rather than failing.
        std::fs::write(&log, vec![b'y'; 32]).unwrap();
        rotate_oversized_log(&log, 16);
        assert_eq!(std::fs::read(&old).unwrap(), vec![b'y'; 32]);

        // Missing file: no-op, no panic.
        rotate_oversized_log(&log, 16);
    }

    /// The credential must reach the drain in its environment and nowhere
    /// near its argv. A command line is world-readable through the process
    /// table for as long as the process lives, so passing the token as a flag
    /// would publish it to every local user each time a session ends.
    #[test]
    fn the_live_token_travels_in_the_environment_not_the_command_line() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = command_spec(tmp.path(), Some("SECRET-BEARER")).unwrap();

        assert_eq!(
            spec.live_token.as_deref(),
            Some(std::ffi::OsStr::new("SECRET-BEARER"))
        );
        let rendered: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            !rendered.iter().any(|a| a.contains("SECRET-BEARER")),
            "token leaked into argv: {rendered:?}"
        );
        assert!(
            rendered.iter().any(|a| a == "hook-drain"),
            "still the drain command: {rendered:?}"
        );
    }

    /// An absent or empty token carries nothing, so a no-auth deployment does
    /// not export an empty bearer into the child.
    #[test]
    fn an_absent_or_empty_token_is_not_carried() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(command_spec(tmp.path(), None).unwrap().live_token.is_none());
        assert!(
            command_spec(tmp.path(), Some(""))
                .unwrap()
                .live_token
                .is_none()
        );
    }

    #[test]
    fn spawn_config_redirects_stdio_and_logs_under_data_dir_logs() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = command_spec(tmp.path(), None).unwrap();
        let config = spawn_config(&spec, true);

        assert!(config.stdin_null);
        assert!(config.stdout_null);
        assert_eq!(
            config.stderr_file,
            tmp.path().join("logs").join("hook-drain.log")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_spawn_config_prefers_true_session_detach_when_available() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = command_spec(tmp.path(), None).unwrap();
        let DetachConfig::UnixNewSession { setsid } = spawn_config(&spec, true).detach;
        if let Some(path) = setsid {
            assert!(
                path == Path::new("/usr/bin/setsid") || path == Path::new("/bin/setsid"),
                "only trusted absolute setsid launchers are used: {path:?}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_spawn_config_uses_expected_flags_and_breakaway_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = command_spec(tmp.path(), None).unwrap();
        let DetachConfig::WindowsCreationFlags(with_breakaway) = spawn_config(&spec, true).detach;
        let DetachConfig::WindowsCreationFlags(without_breakaway) =
            spawn_config(&spec, false).detach;
        assert_ne!(with_breakaway, without_breakaway);
    }
}
