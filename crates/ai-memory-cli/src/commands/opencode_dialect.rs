//! OpenCode major-version resolution shared by launch and installers.

use std::ffi::OsStr;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use ai_memory_workstream::OpenCodeDialect;
use anyhow::{Context as _, Result, anyhow, bail};

use crate::cli::{AgentChoice, McpClient};
use crate::commands::run::EffectiveChildEnv;

const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_VERSION_OUTPUT: u64 = 64 * 1024;

#[derive(Debug)]
pub(crate) struct ResolvedOpenCode {
    pub(crate) dialect: OpenCodeDialect,
    pub(crate) executable: std::path::PathBuf,
}

pub(crate) fn resolve_agent(agent: AgentChoice, env: &EffectiveChildEnv) -> Result<AgentChoice> {
    if agent != AgentChoice::OpenCode {
        return Ok(agent);
    }
    match resolve_generic(None, env)?.dialect {
        OpenCodeDialect::V1 => Ok(AgentChoice::OpenCode),
        OpenCodeDialect::V2 => Ok(AgentChoice::OpenCode2),
    }
}

pub(crate) fn resolve_client(client: McpClient, env: &EffectiveChildEnv) -> Result<McpClient> {
    if client != McpClient::OpenCode {
        return Ok(client);
    }
    match resolve_generic(None, env)?.dialect {
        OpenCodeDialect::V1 => Ok(McpClient::OpenCode),
        OpenCodeDialect::V2 => Ok(McpClient::OpenCode2),
    }
}

pub(crate) fn resolve_generic(
    executable: Option<&OsStr>,
    env: &EffectiveChildEnv,
) -> Result<ResolvedOpenCode> {
    let program = executable.unwrap_or_else(|| OsStr::new("opencode"));
    let resolved = super::run::resolve_program(program, env.executable_search()).ok_or_else(|| {
        anyhow!(
            "OpenCode executable `{}` was not found; install it or select an explicit opencode2 compatibility alias",
            program.to_string_lossy()
        )
    })?;
    let mut command = Command::new(&resolved);
    env.apply_to_std(&mut command);
    let mut child = command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("probing OpenCode executable {}", resolved.display()))?;
    let stdout = read_bounded(child.stdout.take());
    let stderr = read_bounded(child.stderr.take());
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("waiting for OpenCode version probe {}", resolved.display()))?
        {
            break status;
        }
        if started.elapsed() >= PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(timeout_error(&resolved));
        }
        thread::sleep(Duration::from_millis(10));
    };
    let remaining = PROBE_TIMEOUT.saturating_sub(started.elapsed());
    let stdout = stdout
        .recv_timeout(remaining)
        .map_err(|_| timeout_error(&resolved))??;
    let remaining = PROBE_TIMEOUT.saturating_sub(started.elapsed());
    let stderr = stderr
        .recv_timeout(remaining)
        .map_err(|_| timeout_error(&resolved))??;
    if stdout.len() > MAX_VERSION_OUTPUT as usize || stderr.len() > MAX_VERSION_OUTPUT as usize {
        bail!("OpenCode version probe returned more than {MAX_VERSION_OUTPUT} bytes");
    }
    if !status.success() {
        bail!(
            "OpenCode version probe `{}` --version failed with {status}: {}",
            resolved.display(),
            String::from_utf8_lossy(&stderr).trim()
        );
    }
    let stdout = std::str::from_utf8(&stdout).context("OpenCode version output was not UTF-8")?;
    let stderr =
        std::str::from_utf8(&stderr).context("OpenCode version error output was not UTF-8")?;
    let output = match (stdout.trim(), stderr.trim()) {
        ("", "") => bail!("OpenCode version probe returned no output"),
        (stdout, "") => stdout.to_string(),
        ("", stderr) => stderr.to_string(),
        (stdout, stderr) => format!("{stdout}\n{stderr}"),
    };
    let dialect = OpenCodeDialect::parse_version_output(&output).with_context(|| {
        format!(
            "could not determine the OpenCode major from `{}` --version output {:?}; expected a semantic version with major 1 or 2",
            resolved.display(),
            output.trim()
        )
    })?;
    Ok(ResolvedOpenCode {
        dialect,
        executable: resolved,
    })
}

fn timeout_error(program: &std::path::Path) -> anyhow::Error {
    anyhow!(
        "OpenCode version probe timed out after {} seconds for {}; run `{} --version` and verify it exits promptly",
        PROBE_TIMEOUT.as_secs(),
        program.display(),
        program.display()
    )
}

fn read_bounded(
    pipe: Option<impl Read + Send + 'static>,
) -> mpsc::Receiver<std::io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let mut bytes = Vec::new();
            if let Some(pipe) = pipe {
                pipe.take(MAX_VERSION_OUTPUT + 1).read_to_end(&mut bytes)?;
            }
            Ok(bytes)
        })();
        let _ = sender.send(result);
    });
    receiver
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use ai_memory_workstream::ManagedHarness;

    use super::*;

    #[test]
    fn explicit_v2_alias_choices_force_v2_without_probing() {
        assert_eq!(
            resolve_agent(
                AgentChoice::OpenCode2,
                &EffectiveChildEnv::from_runtime(&crate::config::RuntimeEnv::default()),
            )
            .unwrap(),
            AgentChoice::OpenCode2
        );
        assert_eq!(
            resolve_client(
                McpClient::OpenCode2,
                &EffectiveChildEnv::from_runtime(&crate::config::RuntimeEnv::default()),
            )
            .unwrap(),
            McpClient::OpenCode2
        );
    }

    #[test]
    fn version_reader_caps_captured_output() {
        let output = read_bounded(Some(std::io::Cursor::new(vec![
            b'x';
            MAX_VERSION_OUTPUT as usize
                + 128
        ])))
        .recv_timeout(Duration::from_secs(1))
        .unwrap()
        .unwrap();
        assert_eq!(output.len(), MAX_VERSION_OUTPUT as usize + 1);
    }

    #[cfg(unix)]
    fn fake_version(version: &str) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("opencode");
        std::fs::write(
            &executable,
            format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(executable, permissions).unwrap();
        temp
    }

    #[cfg(unix)]
    #[test]
    fn exact_fake_executable_selects_each_dialect() {
        for (version, expected) in [
            ("1.18.34", ManagedHarness::OpenCode),
            ("opencode v2.0.23 (build abc)", ManagedHarness::OpenCode2),
        ] {
            let temp = fake_version(version);
            assert_eq!(
                resolve_generic(
                    Some(temp.path().join("opencode").as_os_str()),
                    &EffectiveChildEnv::from_runtime(&crate::config::RuntimeEnv::default()),
                )
                .unwrap()
                .dialect
                .harness(),
                expected
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_uses_the_effective_child_environment() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("opencode");
        std::fs::write(
            &executable,
            "#!/bin/sh\n[ -z \"${HOME+x}\" ] || { printf 'ambient leak\\n'; exit 1; }\nprintf '%s.%s.0\\n' \"$OPENCODE_MAJOR\" \"$OPENCODE_MINOR\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = EffectiveChildEnv::for_tests([
            ("PATH", temp.path().as_os_str()),
            ("OPENCODE_MAJOR", OsStr::new("2")),
            ("OPENCODE_MINOR", OsStr::new("7")),
        ]);

        let resolved = resolve_generic(None, &env).unwrap();
        assert_eq!(resolved.dialect, OpenCodeDialect::V2);
        assert_eq!(resolved.executable, executable);
    }

    #[cfg(unix)]
    #[test]
    fn version_probe_timeout_fails_actionably() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("opencode");
        std::fs::write(&executable, "#!/bin/sh\nexec sleep 5\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let error = resolve_generic(
            Some(executable.as_os_str()),
            &EffectiveChildEnv::from_runtime(&crate::config::RuntimeEnv::default()),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("version probe timed out"), "{error}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_explicit_extensionless_path_resolves_the_cmd_launcher() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("opencode-custom");
        std::fs::write(
            base.with_extension("cmd"),
            "@echo off\r\necho opencode v2.0.23\r\n",
        )
        .unwrap();
        let resolved = resolve_generic(
            Some(base.as_os_str()),
            &EffectiveChildEnv::from_runtime(&crate::config::RuntimeEnv::default()),
        )
        .unwrap();
        assert_eq!(resolved.dialect, OpenCodeDialect::V2);
        // PATHEXT spells extensions in whatever case the machine defines
        // (`.CMD` on stock Windows), and Windows paths are case-insensitive.
        assert_eq!(
            resolved.executable.to_string_lossy().to_lowercase(),
            base.with_extension("cmd").to_string_lossy().to_lowercase()
        );
    }

    #[cfg(unix)]
    #[test]
    fn malformed_and_future_fake_versions_fail_actionably() {
        for version in ["OpenCode latest", "3.0.0"] {
            let temp = fake_version(version);
            let error = format!(
                "{:#}",
                resolve_generic(
                    Some(temp.path().join("opencode").as_os_str()),
                    &EffectiveChildEnv::from_runtime(&crate::config::RuntimeEnv::default()),
                )
                .unwrap_err()
            );
            assert!(
                error.contains("could not determine the OpenCode major"),
                "{error}"
            );
        }
    }
}
