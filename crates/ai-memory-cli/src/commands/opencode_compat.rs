//! OpenCode major-version detection shared by managed launch and installers.

use std::ffi::OsStr;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

/// Plugin, MCP, and transcript generation used by an OpenCode executable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenCodeGeneration {
    V1,
    V2,
}

/// Detect the API generation exposed by the exact executable that will launch.
pub(crate) fn detect(program: &OsStr) -> Result<OpenCodeGeneration> {
    let program = super::run::resolve_program(program)
        .unwrap_or_else(|| std::path::Path::new(program).into());
    detect_resolved(&program, &[])
}

/// Detect OpenCode through the launch environment (`run --env` / `--env-file`
/// over the current process), so a caller-specific PATH selects and probes the
/// same executable that the child will eventually start.
pub(crate) fn detect_with_env(
    program: &OsStr,
    env_overrides: &[(String, String)],
) -> Result<OpenCodeGeneration> {
    let program = super::run::resolve_program_with_env(program, env_overrides)
        .unwrap_or_else(|| std::path::Path::new(program).into());
    detect_resolved(&program, env_overrides)
}

fn detect_resolved(
    program: &std::path::Path,
    env_overrides: &[(String, String)],
) -> Result<OpenCodeGeneration> {
    let mut command = Command::new(program);
    command.arg("--version");
    for (key, value) in env_overrides {
        command.env(key, value);
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "running `{}` --version to select a compatible OpenCode integration",
                program.display()
            )
        })?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait()? {
            Some(_) => break,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!(
                    "`{}` --version did not finish within 2 seconds; refusing to guess the OpenCode plugin API",
                    program.display()
                );
            }
        }
    }
    let output = child.wait_with_output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = format!("{stdout}\n{stderr}");
    let generation = parse_version_output(&text).with_context(|| {
        format!(
            "could not determine the OpenCode major version from `{}` --version output; use the explicit `opencode2` compatibility selector for OpenCode V2 or verify the installed executable",
            program.display()
        )
    })?;
    if !output.status.success() {
        bail!(
            "`{}` --version exited with {}; refusing to guess the OpenCode plugin API",
            program.display(),
            output.status
        );
    }
    Ok(generation)
}

fn parse_version_output(output: &str) -> Result<OpenCodeGeneration> {
    for token in output.split_whitespace() {
        let candidate = token
            .trim_matches(|character: char| {
                !character.is_ascii_alphanumeric() && character != '.' && character != '-'
            })
            .trim_start_matches(['v', 'V']);
        let Some((major, _)) = candidate.split_once('.') else {
            continue;
        };
        let Ok(major) = major.parse::<u64>() else {
            continue;
        };
        return match major {
            1 => Ok(OpenCodeGeneration::V1),
            2 => Ok(OpenCodeGeneration::V2),
            _ => Err(anyhow!(
                "unsupported OpenCode major version {major}; ai-memory currently supports versions 1 and 2"
            )),
        };
    }
    Err(anyhow!("version output contained no semantic version"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_opencode_version_forms() {
        for (output, expected) in [
            ("opencode 1.2.3", OpenCodeGeneration::V1),
            ("opencode v1.9.0", OpenCodeGeneration::V1),
            ("opencode v2.0.21", OpenCodeGeneration::V2),
            ("2.4.0\n", OpenCodeGeneration::V2),
        ] {
            assert_eq!(parse_version_output(output).unwrap(), expected, "{output}");
        }
    }

    #[test]
    fn rejects_ambiguous_and_unsupported_versions() {
        assert!(parse_version_output("opencode development build").is_err());
        assert!(parse_version_output("opencode v3.0.0").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn probes_the_exact_executable_and_accepts_stderr_version_output() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("custom-opencode");
        std::fs::write(&executable, "#!/bin/sh\necho 'opencode v2.3.4' >&2\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        assert_eq!(
            detect(executable.as_os_str()).unwrap(),
            OpenCodeGeneration::V2
        );
    }

    #[cfg(unix)]
    #[test]
    fn launch_environment_path_selects_the_probed_executable() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("opencode");
        std::fs::write(&executable, "#!/bin/sh\necho 'opencode 1.8.0'\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        assert_eq!(
            detect_with_env(
                OsStr::new("opencode"),
                &[("PATH".into(), temp.path().display().to_string())],
            )
            .unwrap(),
            OpenCodeGeneration::V1
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_failed_version_probe_even_when_it_prints_a_version() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("broken-opencode");
        std::fs::write(&executable, "#!/bin/sh\necho 'opencode v1.9.0'\nexit 1\n").unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let error = detect(executable.as_os_str()).unwrap_err().to_string();
        assert!(error.contains("exited with"), "{error}");
    }
}
