//! Packaging asset regression tests.

#[cfg(unix)]
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
#[cfg(any(unix, windows))]
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;

#[cfg(unix)]
fn sha256_file(path: &Path) -> String {
    use sha2::{Digest as _, Sha256};

    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crate should live under crates/ai-memory-cli")
        .to_path_buf()
}

fn read_repo(path: &str) -> String {
    let path = repo_root().join(path);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

// Unix-only alongside run_wrapper_on_fake_macos below — these helpers'
// former Git Bash arms existed to run the wrapper test on Windows, which
// the fake-uname executable-bit limitation rules out anyway.
#[cfg(unix)]
fn shell_script_command(script: &Path) -> Command {
    Command::new(script)
}

#[cfg(unix)]
// Preserve the script path as `$0` without directly execing a just-written file,
// which can transiently return ETXTBSY under parallel Linux test load.
fn freshly_written_shell_script_command(script: &Path) -> Command {
    let mut command = Command::new("bash");
    command.arg(script);
    command
}

#[cfg(unix)]
fn shell_path(path: &Path) -> String {
    path.display().to_string()
}

// Unix-only: the macOS simulation works by shadowing `uname` with a fake
// script earlier in PATH, which requires setting its executable bit. NTFS
// has no mode bits, so on a Windows host MSYS bash skips the non-executable
// fake and the real `uname.exe` reports MSYS_NT-* — the Darwin arm under
// test can never fire there.
#[cfg(unix)]
fn run_wrapper_on_fake_macos(args: &[&str]) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let docker_args = tmp.path().join("docker-args.txt");
    let docker = tmp.path().join("docker");
    let uname = tmp.path().join("uname");
    std::fs::write(
        &docker,
        format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > {}\n",
            shell_path(&docker_args)
        ),
    )
    .unwrap();
    std::fs::write(&uname, "#!/usr/bin/env bash\nprintf 'Darwin\\n'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&uname, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let path = format!(
        "{}:{}",
        shell_path(tmp.path()),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = shell_script_command(&repo_root().join("bin/ai-memory"));
    let output = command
        .args(args)
        .env("PATH", path)
        .env("AI_MEMORY_DOCKER", shell_path(&docker))
        .env("AI_MEMORY_NO_VERSION_CHECK", "1")
        .env("AI_MEMORY_DATA_VOLUME", "test-ai-memory-data")
        .env("HOME", shell_path(tmp.path()))
        .env_remove("AI_MEMORY_SERVER_URL")
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "wrapper failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(docker_args).unwrap()
}

#[test]
fn systemd_units_use_explicit_native_paths() {
    let system = read_repo("packaging/systemd/ai-memory.service");
    assert!(system.contains("--data-dir /var/lib/ai-memory"));
    assert!(system.contains("--config /etc/ai-memory/config.toml"));
    assert!(system.contains("EnvironmentFile=-/etc/ai-memory/env"));
    assert!(system.contains("StateDirectory=ai-memory"));
    assert!(system.contains("ReadWritePaths=/var/lib/ai-memory"));
    assert!(!system.contains("/var/local"));

    let user = read_repo("packaging/systemd/ai-memory-user.service");
    assert!(user.contains("--data-dir %h/.local/share/ai-memory"));
    assert!(user.contains("--config %h/.config/ai-memory/config.toml"));
    assert!(user.contains("EnvironmentFile=-%h/.config/ai-memory/env"));
    assert!(!user.contains("/var/lib/ai-memory"));
}

#[test]
fn aur_packages_install_all_native_assets() {
    for path in ["packaging/aur/PKGBUILD", "packaging/aur/PKGBUILD-bin"] {
        let pkgbuild = read_repo(path);
        assert!(pkgbuild.contains("/usr/bin/ai-memory"), "{path}");
        assert!(pkgbuild.contains("/usr/share/ai-memory"), "{path}");
        assert!(
            pkgbuild.contains("/usr/lib/systemd/system/ai-memory.service"),
            "{path}"
        );
        assert!(
            pkgbuild.contains("/usr/lib/systemd/user/ai-memory.service"),
            "{path}"
        );
        assert!(
            pkgbuild.contains("/usr/lib/sysusers.d/ai-memory.conf"),
            "{path}"
        );
        assert!(
            pkgbuild.contains("/usr/lib/tmpfiles.d/ai-memory.conf"),
            "{path}"
        );
        assert!(pkgbuild.contains("etc/ai-memory/config.toml"), "{path}");
        assert!(pkgbuild.contains("etc/ai-memory/env"), "{path}");
        assert!(
            pkgbuild.contains("install -Dm0640 packaging/env/ai-memory.env"),
            "{path}"
        );
    }

    let install = read_repo("packaging/aur/ai-memory.install");
    assert!(install.contains("sudo -u ai-memory ai-memory --data-dir /var/lib/ai-memory"));
    assert!(!install.contains("sudo ai-memory --data-dir /var/lib/ai-memory"));

    let bin_pkgbuild = read_repo("packaging/aur/PKGBUILD-bin");
    assert!(bin_pkgbuild.contains("source_x86_64"));
    assert!(bin_pkgbuild.contains("source_aarch64"));
    assert!(bin_pkgbuild.contains("linux-x86_64.tar.gz"));
    assert!(bin_pkgbuild.contains("linux-aarch64.tar.gz"));
}

#[test]
fn docker_source_build_uses_vendored_tailwind() {
    let dockerfile = read_repo("docker/Dockerfile");
    assert!(dockerfile.contains("TAILWIND_SKIP=1 cargo build --release -p ai-memory-cli"));
}

#[test]
fn docker_context_excludes_operator_deployment_files() {
    let dockerignore = read_repo(".dockerignore");
    let protected_suffix = concat!(
        "# Operator-specific deployment files. Keep this block last so a later negation\n",
        "# cannot re-include credentials or host configuration in the build context.\n",
        "/bin/deploy.env\n",
        "/docker/.env.production\n",
        "/docker/docker-compose.prod.yml\n",
    );

    assert!(
        dockerignore.ends_with(protected_suffix),
        "operator deployment exclusions must remain the final Docker ignore rules"
    );
}

#[test]
fn docker_publish_jobs_use_prebuilt_binaries() {
    let dockerfile = read_repo("docker/Dockerfile");
    assert!(dockerfile.contains("FROM runtime-base AS runtime-prebuilt-amd64"));
    assert!(dockerfile.contains("FROM runtime-base AS runtime-prebuilt-arm64"));
    assert!(dockerfile.contains("dist/docker/ai-memory-linux-x86_64/ai-memory"));
    assert!(dockerfile.contains("dist/docker/ai-memory-linux-aarch64/ai-memory"));

    let release = read_repo(".github/workflows/release.yml");
    assert!(release.contains("artifact: ai-memory-linux-x86_64"));
    assert!(release.contains("artifact: ai-memory-linux-aarch64"));
    assert!(release.contains("artifact: ai-memory-macos-aarch64"));
    assert!(release.contains("artifact: ai-memory-macos-x86_64"));
    assert!(release.contains("needs: [binary, macos, windows, validate-version]"));
    assert!(release.contains("target: runtime-prebuilt-amd64"));
    assert!(release.contains("target: runtime-prebuilt-arm64"));

    let ci = read_repo(".github/workflows/ci.yml");
    assert!(ci.contains("ci-ai-memory-${{ matrix.artifact }}"));
    assert!(ci.contains("artifact: linux-x86_64"));
    assert!(ci.contains("artifact: macos-aarch64"));
    assert!(ci.contains("artifact: macos-x86_64"));
    assert!(ci.contains("runner: macos-15"));
    assert!(ci.contains("runner: macos-15-intel"));
    assert!(ci.contains("--target runtime-prebuilt-amd64"));
}

#[cfg(unix)]
#[test]
fn macos_wrapper_routes_urls_by_real_subcommand() {
    for subcommand in ["install-mcp", "install-hooks", "setup-agent"] {
        let args = run_wrapper_on_fake_macos(&[subcommand]);
        assert!(
            !args.contains("AI_MEMORY_SERVER_URL=http://host.docker.internal:49374"),
            "{subcommand} renders host-side config and must keep loopback defaults; got {args}"
        );
    }

    let args = run_wrapper_on_fake_macos(&["status"]);
    assert!(
        args.contains("AI_MEMORY_SERVER_URL=http://host.docker.internal:49374"),
        "thin-client commands must reach the host server through Docker Desktop; got {args}"
    );

    let args = run_wrapper_on_fake_macos(&["search", "install-hooks"]);
    assert!(
        args.contains("AI_MEMORY_SERVER_URL=http://host.docker.internal:49374"),
        "only the actual subcommand should control URL routing; got {args}"
    );

    let args = run_wrapper_on_fake_macos(&["--config", "/tmp/config.toml", "install-hooks"]);
    assert!(
        !args.contains("AI_MEMORY_SERVER_URL=http://host.docker.internal:49374"),
        "global options before install-hooks must not hide the real subcommand; got {args}"
    );
}

// Like run_wrapper_on_fake_macos's fake docker, but the wrapper is spawned with
// stdin on a pipe: the shape every `cat page.md | ai-memory write-page --body -`
// (and every CI/cron) invocation actually has.
#[cfg(unix)]
fn run_wrapper_with_piped_stdin(args: &[&str], stdin_payload: &str) -> String {
    use std::io::Write as _;

    let tmp = tempfile::tempdir().unwrap();
    let docker_args = tmp.path().join("docker-args.txt");
    let docker = tmp.path().join("docker");
    std::fs::write(
        &docker,
        format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > {}\n",
            shell_path(&docker_args)
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut child = shell_script_command(&repo_root().join("bin/ai-memory"))
        .args(args)
        .env("AI_MEMORY_DOCKER", shell_path(&docker))
        .env("AI_MEMORY_NO_VERSION_CHECK", "1")
        .env("AI_MEMORY_DATA_VOLUME", "test-ai-memory-data")
        .env("HOME", shell_path(tmp.path()))
        .env_remove("AI_MEMORY_SERVER_URL")
        .env_remove("CLAUDE_CONFIG_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(stdin_payload.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "wrapper failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(docker_args).unwrap()
}

#[cfg(unix)]
#[test]
fn wrapper_keeps_stdin_attached_when_it_is_a_pipe() {
    let args = run_wrapper_with_piped_stdin(
        &["write-page", "--path", "notes/x.md", "--body", "-"],
        "# body that must survive the container boundary\n",
    );
    let flags: Vec<&str> = args.lines().collect();

    // Without `-i` docker gives the container a closed stdin, so `--body -`
    // reads an empty string and the page is persisted with frontmatter only —
    // silently, because the CLI still reports a successful write.
    assert!(
        flags.contains(&"-i"),
        "piped stdin must stay attached for `--body -`; got {args}"
    );
    // A pipe is not a terminal: asking for a TTY here makes docker fail with
    // "the input device is not a TTY".
    assert!(
        !flags.contains(&"-t") && !flags.contains(&"-it"),
        "no TTY may be requested when stdin is a pipe; got {args}"
    );
    assert!(
        flags
            .iter()
            .any(|arg| arg.starts_with("AI_MEMORY_SCOPE_CWD=/scope")),
        "an outside-home checkout must expose its bounded marker path; got {args}"
    );
    assert!(
        flags.iter().any(|arg| arg.ends_with(":/scope:ro")),
        "the marker scope mount must be read-only; got {args}"
    );
}

#[test]
fn docker_wrappers_keep_stdin_attached_independently_of_tty_allocation() {
    let posix = read_repo("bin/ai-memory");
    assert!(
        posix.contains("TTY_ARGS=(-i)"),
        "POSIX wrapper must attach stdin before inspecting terminal state"
    );
    assert!(
        posix.contains("TTY_ARGS+=(-t)"),
        "POSIX wrapper must add TTY allocation separately"
    );
    assert!(
        !posix.contains("TTY_ARGS=(-it)"),
        "combined flags can drop stdin when only stdout is redirected"
    );
    assert!(
        posix.contains("CLAUDE_CODE_SESSION_ID"),
        "POSIX wrapper must forward Claude's session id into the bridge container"
    );
    assert!(posix.contains("git -C \"${PWD}\" rev-parse --show-toplevel"));
    assert!(posix.contains("AI_MEMORY_SCOPE_CWD=/scope${SCOPE_REL}"));
    assert!(posix.contains("${SCOPE_ROOT}:/scope:ro"));

    let powershell = read_repo("bin/ai-memory.ps1");
    assert!(
        powershell.contains("$DockerArgs = @(\"run\", \"--rm\", \"-i\")"),
        "PowerShell wrapper must attach stdin before inspecting console state"
    );
    assert!(
        powershell.contains("$DockerArgs += \"-t\""),
        "PowerShell wrapper must add TTY allocation separately"
    );
    assert!(
        !powershell.contains("$DockerArgs += \"-it\""),
        "PowerShell must not couple stdin attachment to TTY allocation"
    );
    assert!(
        powershell.contains("\"CLAUDE_CODE_SESSION_ID\""),
        "PowerShell wrapper must forward Claude's session id into the bridge container"
    );
    assert!(powershell.contains("AI_MEMORY_SCOPE_CWD=$ScopeCwd"));
    assert!(powershell.contains("${ScopeRoot}:/scope:ro"));
}

#[test]
fn wrapper_updates_and_install_docs_use_verified_release_assets() {
    let wrapper = read_repo("bin/ai-memory");
    assert!(wrapper.contains("releases/latest/download/ai-memory-wrapper"));
    assert!(wrapper.contains("WRAPPER_SHA256_URL"));
    assert!(wrapper.contains("wrapper checksum mismatch; refusing update"));
    assert!(!wrapper.contains("raw.githubusercontent.com/akitaonrails/ai-memory/main"));

    let release = read_repo(".github/workflows/release.yml");
    for asset in [
        "ai-memory-wrapper",
        "ai-memory-wrapper.ps1",
        "ai-memory-wrapper.cmd",
        "ai-memory-install-hooks",
        "ai-memory-hooks.tar.gz",
    ] {
        assert!(release.contains(asset), "release must publish {asset}");
        assert!(
            release.contains(&format!("{asset}.sha256")),
            "release must publish a checksum for {asset}"
        );
    }
    assert!(release.contains("permissions:\n  contents: read"));
    assert!(release.contains("github-release:"));
    assert!(release.contains("contents: write"));

    for path in ["README.md", "docs/install.md", "docs/windows.md"] {
        let docs = read_repo(path);
        assert!(
            !docs.contains("raw.githubusercontent.com/akitaonrails/ai-memory/main/bin/ai-memory"),
            "{path} must not install an executable from mutable main"
        );
    }
    let install_docs = read_repo("docs/install.md");
    assert!(
        !install_docs.contains("raw.githubusercontent.com/akitaonrails/ai-memory/main/scripts"),
        "hook installation docs must not execute mutable main"
    );
    let hook_installer = read_repo("scripts/install-hooks.sh");
    assert!(hook_installer.contains("ARCHIVE=\"ai-memory-hooks.tar.gz\""));
    assert!(hook_installer.contains("$BASE_URL/$ARCHIVE.sha256"));
    assert!(hook_installer.contains("hook bundle checksum mismatch; refusing installation"));
    assert!(hook_installer.contains("tar -xOf"));
    assert!(!hook_installer.contains("tar -xzf"));
    assert!(!hook_installer.contains("raw.githubusercontent.com"));
}

#[test]
fn github_actions_are_pinned_to_full_commits() {
    for path in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
        let workflow = read_repo(path);
        for line in workflow.lines().filter(|line| line.contains("uses: ")) {
            let Some(reference) = line
                .split('@')
                .nth(1)
                .and_then(|value| value.split_whitespace().next())
            else {
                continue;
            };
            assert!(
                reference.len() == 40 && reference.chars().all(|c| c.is_ascii_hexdigit()),
                "{path} action is not pinned to a full commit: {line}"
            );
        }
    }
}

#[test]
fn workflows_keep_fixed_rust_jobs_on_the_fixed_toolchain() {
    for (path, expected_fixed_jobs) in [
        (".github/workflows/ci.yml", 1),
        (".github/workflows/release.yml", 3),
    ] {
        let workflow = read_repo(path);
        let lines = workflow.lines().collect::<Vec<_>>();
        let mut fixed_jobs = 0;
        for (index, line) in lines.iter().enumerate().filter(|(_, line)| {
            line.contains("uses: dtolnay/rust-toolchain@") && line.ends_with("# 1.95")
        }) {
            fixed_jobs += 1;
            assert_eq!(
                lines.get(index + 1).map(|line| line.trim()),
                Some("with:"),
                "{path} must configure the fixed toolchain after: {line}"
            );
            assert_eq!(
                lines.get(index + 2).map(|line| line.trim()),
                Some("toolchain: \"1.95\""),
                "{path} must keep its # 1.95 job on Rust 1.95"
            );
        }

        assert_eq!(
            fixed_jobs, expected_fixed_jobs,
            "{path} has an unexpected number of fixed Rust jobs"
        );
    }
}

#[cfg(unix)]
#[test]
fn wrapper_self_upgrade_rejects_a_checksum_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let wrapper = bin_dir.join("ai-memory");
    let original = read_repo("bin/ai-memory");
    std::fs::write(&wrapper, &original).unwrap();

    let payload = tmp.path().join("hostile-wrapper");
    std::fs::write(
        &payload,
        "#!/usr/bin/env bash\nprintf 'hostile payload executed\\n' >&2\nexit 91\n",
    )
    .unwrap();
    let curl = bin_dir.join("curl");
    std::fs::write(
        &curl,
        "#!/usr/bin/env bash\n\
         set -euo pipefail\n\
         url=''\n\
         out=''\n\
         while [ \"$#\" -gt 0 ]; do\n\
           case \"$1\" in\n\
             -o) out=\"$2\"; shift 2 ;;\n\
             -*) shift ;;\n\
             *) url=\"$1\"; shift ;;\n\
           esac\n\
         done\n\
         case \"$url\" in\n\
           *.sha256) printf '%064d  ai-memory-wrapper\\n' 0 > \"$out\" ;;\n\
           *) cp \"$FAKE_WRAPPER_PAYLOAD\" \"$out\" ;;\n\
         esac\n",
    )
    .unwrap();
    let docker = bin_dir.join("docker");
    std::fs::write(&docker, "#!/usr/bin/env bash\nexit 0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        for path in [&wrapper, &payload, &curl, &docker] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    let path = format!(
        "{}:{}",
        shell_path(&bin_dir),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = freshly_written_shell_script_command(&wrapper)
        .arg("upgrade")
        .env("PATH", path)
        .env("HOME", tmp.path())
        .env("AI_MEMORY_DOCKER", &docker)
        .env(
            "AI_MEMORY_WRAPPER_URL",
            "https://example.invalid/ai-memory-wrapper",
        )
        .env("FAKE_WRAPPER_PAYLOAD", &payload)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "upgrade failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("wrapper checksum mismatch; refusing update"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("hostile payload executed"));
    assert_eq!(std::fs::read_to_string(&wrapper).unwrap(), original);
}

#[cfg(unix)]
#[test]
fn wrapper_self_upgrade_installs_and_runs_a_verified_payload() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let wrapper = bin_dir.join("ai-memory");
    std::fs::write(&wrapper, read_repo("bin/ai-memory")).unwrap();

    let payload = tmp.path().join("verified-wrapper");
    let payload_body = "#!/usr/bin/env bash\nprintf 'verified wrapper executed\\n'\n";
    std::fs::write(&payload, payload_body).unwrap();
    let curl = bin_dir.join("curl");
    std::fs::write(
        &curl,
        "#!/usr/bin/env bash\n\
         set -euo pipefail\n\
         url=''\n\
         out=''\n\
         while [ \"$#\" -gt 0 ]; do\n\
           case \"$1\" in\n\
             -o) out=\"$2\"; shift 2 ;;\n\
             -*) shift ;;\n\
             *) url=\"$1\"; shift ;;\n\
           esac\n\
         done\n\
         case \"$url\" in\n\
           *.sha256) printf '%s  ai-memory-wrapper\\n' \"$FAKE_WRAPPER_CHECKSUM\" > \"$out\" ;;\n\
           *) cp \"$FAKE_WRAPPER_PAYLOAD\" \"$out\" ;;\n\
         esac\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        for path in [&wrapper, &payload, &curl] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    let path = format!(
        "{}:{}",
        shell_path(&bin_dir),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = freshly_written_shell_script_command(&wrapper)
        .arg("upgrade")
        .env("PATH", path)
        .env("HOME", tmp.path())
        .env(
            "AI_MEMORY_WRAPPER_URL",
            "https://example.invalid/ai-memory-wrapper",
        )
        .env("FAKE_WRAPPER_PAYLOAD", &payload)
        .env("FAKE_WRAPPER_CHECKSUM", sha256_file(&payload))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "upgrade failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("verified wrapper executed"));
    assert_eq!(std::fs::read_to_string(&wrapper).unwrap(), payload_body);
}

#[cfg(unix)]
#[test]
fn hook_installer_rejects_a_checksum_mismatch_before_writing_scripts() {
    let tmp = tempfile::tempdir().unwrap();
    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let curl = bin_dir.join("curl");
    std::fs::write(
        &curl,
        "#!/usr/bin/env bash\n\
         set -euo pipefail\n\
         url=''\n\
         out=''\n\
         while [ \"$#\" -gt 0 ]; do\n\
           case \"$1\" in\n\
             -o) out=\"$2\"; shift 2 ;;\n\
             -*) shift ;;\n\
             *) url=\"$1\"; shift ;;\n\
           esac\n\
         done\n\
         case \"$url\" in\n\
           *.sha256) printf '%064d  ai-memory-hooks.tar.gz\\n' 0 > \"$out\" ;;\n\
           *) printf 'not the expected archive' > \"$out\" ;;\n\
         esac\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        shell_path(&bin_dir),
        std::env::var("PATH").unwrap_or_default()
    );
    let destination = tmp.path().join("hooks");
    let output = shell_script_command(&repo_root().join("scripts/install-hooks.sh"))
        .args(["--agent", "claude-code", "--to"])
        .arg(&destination)
        .env("PATH", path)
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "checksum mismatch must fail closed"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("hook bundle checksum mismatch; refusing installation")
    );
    let agent_dir = destination.join("claude-code");
    assert!(
        !agent_dir.exists() || std::fs::read_dir(agent_dir).unwrap().next().is_none(),
        "no hook script may be written before archive verification"
    );
}

#[cfg(unix)]
fn installed_hook_names(agent_arg: &str, canonical_agent: &str, hooks: &[&str]) -> Vec<String> {
    let tmp = tempfile::tempdir().unwrap();
    let bundle = tmp.path().join("bundle/hooks").join(canonical_agent);
    std::fs::create_dir_all(&bundle).unwrap();
    for hook in hooks {
        std::fs::write(
            bundle.join(format!("{hook}.sh")),
            format!("#!/usr/bin/env bash\nprintf '{hook}\\n'\n"),
        )
        .unwrap();
    }
    let archive = tmp.path().join("ai-memory-hooks.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(tmp.path().join("bundle"))
        .arg("hooks")
        .status()
        .unwrap();
    assert!(status.success());

    let bin_dir = tmp.path().join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let curl = bin_dir.join("curl");
    std::fs::write(
        &curl,
        "#!/usr/bin/env bash\n\
         set -euo pipefail\n\
         url=''\n\
         out=''\n\
         while [ \"$#\" -gt 0 ]; do\n\
           case \"$1\" in\n\
             -o) out=\"$2\"; shift 2 ;;\n\
             -*) shift ;;\n\
             *) url=\"$1\"; shift ;;\n\
           esac\n\
         done\n\
         case \"$url\" in\n\
           *.sha256) printf '%s  ai-memory-hooks.tar.gz\\n' \"$FAKE_HOOK_CHECKSUM\" > \"$out\" ;;\n\
           *) cp \"$FAKE_HOOK_ARCHIVE\" \"$out\" ;;\n\
         esac\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        shell_path(&bin_dir),
        std::env::var("PATH").unwrap_or_default()
    );
    let destination = tmp.path().join("installed-hooks");
    let output = shell_script_command(&repo_root().join("scripts/install-hooks.sh"))
        .args(["--agent", agent_arg, "--to"])
        .arg(&destination)
        .env("PATH", path)
        .env("HOME", tmp.path())
        .env("FAKE_HOOK_ARCHIVE", &archive)
        .env("FAKE_HOOK_CHECKSUM", sha256_file(&archive))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "installer failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let installed = destination.join(canonical_agent);
    let mut names = std::fs::read_dir(&installed)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    names.sort();
    use std::os::unix::fs::PermissionsExt as _;
    for name in &names {
        assert_ne!(
            std::fs::metadata(installed.join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
    names
}

#[cfg(unix)]
#[test]
fn hook_installer_writes_only_expected_files_from_a_verified_archive() {
    const HOOKS: &[&str] = &[
        "post-tool-use",
        "pre-compact",
        "pre-tool-use",
        "session-end",
        "session-start",
        "stop",
        "user-prompt-submit",
    ];

    let names = installed_hook_names("claude-code", "claude-code", HOOKS);
    let expected = HOOKS
        .iter()
        .map(|hook| format!("{hook}.sh"))
        .collect::<Vec<_>>();
    assert_eq!(names, expected);
}

#[cfg(unix)]
#[test]
fn hook_installer_writes_only_command_code_stable_events() {
    const HOOKS: &[&str] = &["post-tool-use", "pre-tool-use", "session-start", "stop"];

    let names = installed_hook_names("cmdc", "command-code", HOOKS);
    let expected = HOOKS
        .iter()
        .map(|hook| format!("{hook}.sh"))
        .collect::<Vec<_>>();
    assert_eq!(names, expected);
}

#[cfg(unix)]
#[test]
fn managed_host_commands_use_native_path_and_remote_server_without_docker() {
    let tmp = tempfile::tempdir().unwrap();
    let native = tmp.path().join("native-ai-memory");
    let docker = tmp.path().join("docker");
    let record = tmp.path().join("native-record.txt");
    let docker_record = tmp.path().join("docker-record.txt");
    std::fs::write(
        &native,
        format!(
            "#!/usr/bin/env bash\n\
             printf 'server=%s\\nauth=%s\\npath=%s\\n' \"$AI_MEMORY_SERVER_URL\" \"$AI_MEMORY_AUTH_TOKEN\" \"$PATH\" > {}\n\
             printf 'arg=%s\\n' \"$@\" >> {}\n",
            shell_path(&record),
            shell_path(&record)
        ),
    )
    .unwrap();
    std::fs::write(
        &docker,
        format!(
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$@\" > {}\nexit 99\n",
            shell_path(&docker_record)
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&native, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let host_path = format!(
        "{}:{}",
        shell_path(tmp.path()),
        std::env::var("PATH").unwrap_or_default()
    );
    let commands: &[&[&str]] = &[
        &["run", "codex", "--yolo", "resume"],
        &["show", "--json", "--no-scan"],
        &["continue", "--workspace", "work", "--yolo"],
        &["workstreams", "--limit", "5", "--json"],
    ];
    for args in commands {
        let output = shell_script_command(&repo_root().join("bin/ai-memory"))
            .args(args.iter().copied())
            .env("AI_MEMORY_NATIVE_BIN", &native)
            .env("AI_MEMORY_DOCKER", &docker)
            .env("AI_MEMORY_SERVER_URL", "http://192.168.0.90:49374")
            .env("AI_MEMORY_AUTH_TOKEN", "remote-test-token")
            .env("PATH", &host_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "wrapper failed for {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let mut expected = format!(
            "server=http://192.168.0.90:49374\n\
             auth=remote-test-token\n\
             path={host_path}\n"
        );
        for arg in *args {
            expected.push_str(&format!("arg={arg}\n"));
        }
        assert_eq!(std::fs::read_to_string(&record).unwrap(), expected);
    }
    assert!(
        !docker_record.exists(),
        "managed host command entered Docker"
    );
}

#[cfg(unix)]
#[test]
fn wrapper_upgrade_does_not_claim_an_updated_remote_server_is_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let docker = tmp.path().join("docker");
    std::fs::write(
        &docker,
        "#!/usr/bin/env bash\n\
         case \"$1\" in\n\
           pull | ps) exit 0 ;;\n\
           *) exit 1 ;;\n\
         esac\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let output = shell_script_command(&repo_root().join("bin/ai-memory"))
        .arg("upgrade")
        .env("AI_MEMORY_DOCKER", &docker)
        .env("AI_MEMORY_SKIP_SELF_UPGRADE", "1")
        .env("AI_MEMORY_SERVER_URL", "http://192.168.0.90:49374")
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "wrapper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("does not\n  inspect or redeploy the remote server"));
    assert!(stdout.contains("If that host is not already current"));
    assert!(!stdout.contains("remote server still\n  runs the previous version"));
}

#[cfg(unix)]
#[test]
fn docker_wrapper_completions_tolerate_an_early_reader_close() {
    let tmp = tempfile::tempdir().unwrap();
    let docker = tmp.path().join("docker");
    std::fs::write(
        &docker,
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = info ]; then\n\
           printf '[name=seccomp,profile=default]\\n'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = run ]; then\n\
           i=0\n\
           while [ \"$i\" -lt 20000 ]; do\n\
             printf 'complete -c ai-memory -n condition-%s\\n' \"$i\"\n\
             i=$((i + 1))\n\
           done\n\
           exit 0\n\
         fi\n\
         exit 1\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut child = shell_script_command(&repo_root().join("bin/ai-memory"))
        .args(["completions", "fish"])
        .env("AI_MEMORY_DOCKER", &docker)
        .env("AI_MEMORY_NO_TTY", "1")
        .env("AI_MEMORY_NO_VERSION_CHECK", "1")
        .env("AI_MEMORY_DATA_VOLUME", "test-ai-memory-data")
        .env("HOME", tmp.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut first_line = String::new();
    stdout.read_line(&mut first_line).unwrap();
    drop(stdout);

    let output = child.wait_with_output().unwrap();
    assert_eq!(first_line, "complete -c ai-memory -n condition-0\n");
    assert!(
        output.status.success(),
        "early close should stay quiet and successful: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("broken pipe"),
        "wrapper leaked Docker's broken-pipe diagnostic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn docker_wrapper_completions_preserve_helper_failure_without_partial_output() {
    let tmp = tempfile::tempdir().unwrap();
    let docker = tmp.path().join("docker");
    std::fs::write(
        &docker,
        "#!/usr/bin/env bash\n\
         if [ \"$1\" = info ]; then\n\
           printf '[name=seccomp,profile=default]\\n'\n\
           exit 0\n\
         fi\n\
         if [ \"$1\" = run ]; then\n\
           printf 'partial completion output\\n'\n\
           printf 'helper failed\\n' >&2\n\
           exit 42\n\
         fi\n\
         exit 1\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let output = shell_script_command(&repo_root().join("bin/ai-memory"))
        .args(["completions", "fish"])
        .env("AI_MEMORY_DOCKER", &docker)
        .env("AI_MEMORY_NO_TTY", "1")
        .env("AI_MEMORY_NO_VERSION_CHECK", "1")
        .env("AI_MEMORY_DATA_VOLUME", "test-ai-memory-data")
        .env("HOME", tmp.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(42));
    assert!(
        output.stdout.is_empty(),
        "failed helper leaked partial completions: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(String::from_utf8_lossy(&output.stderr), "helper failed\n");
}

// Unlike run_wrapper_on_fake_macos's docker fake (which only ever sees one
// meaningful call — the final `docker run`), the rootless-Docker UID check
// calls `docker info` *before* `docker run`, so this fake must dispatch on
// $1: real stdout for `info` (read by the wrapper's `grep -q rootless`) vs.
// logging argv to a file for `run` (read back by the test).
// How the fake `docker` binary answers `docker info --format …`. The two
// engines expose the same two facts under incompatible keys, and the wrapper
// has to read both, so the fake has to be able to impersonate either one.
#[cfg(unix)]
enum FakeInfo<'a> {
    // Docker: `{{.SecurityOptions}}` prints the security-options list.
    Docker(&'a str),
    // Podman, including the podman-docker `docker` shim: `.SecurityOptions`
    // is not a field of its info report, so the template fails and the
    // command exits non-zero with nothing on stdout. The equivalent facts
    // live under `.Host.Security.*`.
    Podman { rootless: bool, selinux: bool },
}

#[cfg(unix)]
fn run_wrapper_with_fake_docker(args: &[&str], docker_info_stdout: &str) -> String {
    run_wrapper_with_fake_docker_and_uname(args, docker_info_stdout, None)
}

#[cfg(unix)]
fn run_wrapper_with_fake_docker_and_claude_config(
    args: &[&str],
    docker_info_stdout: &str,
    claude_config_dir: &str,
) -> String {
    run_wrapper_with_fake_docker_env(
        args,
        FakeInfo::Docker(docker_info_stdout),
        None,
        Some(claude_config_dir),
        None,
        &[],
    )
}

#[cfg(unix)]
fn run_wrapper_with_fake_docker_and_forwarded_env(
    args: &[&str],
    docker_info_stdout: &str,
    forwarded_env: &[(&str, &str)],
) -> String {
    run_wrapper_with_fake_docker_env(
        args,
        FakeInfo::Docker(docker_info_stdout),
        None,
        None,
        None,
        forwarded_env,
    )
}

// The wrapper also shells out to `id -u` / `id -g` when choosing its default
// Docker uid mapping. Arch container tests often run as root, which would make
// the default mapping `-u 0:0` and produce a false positive in the assertions
// below. Shadow `id` too so these tests exercise the rootless/rootful branch
// logic, not the uid of the test runner. This shadow is unconditional
// (unlike `uname`, which only matters for the macOS-simulation callers)
// because every caller of this helper is exposed to the flakiness.
#[cfg(unix)]
fn run_wrapper_with_fake_docker_and_uname(
    args: &[&str],
    docker_info_stdout: &str,
    uname_stdout: Option<&str>,
) -> String {
    run_wrapper_with_fake_docker_env(
        args,
        FakeInfo::Docker(docker_info_stdout),
        uname_stdout,
        None,
        None,
        &[],
    )
}

#[cfg(unix)]
fn run_wrapper_with_fake_selinux(
    args: &[&str],
    docker_info_stdout: &str,
    selinux_mode: &str,
) -> String {
    run_wrapper_with_fake_docker_env(
        args,
        FakeInfo::Docker(docker_info_stdout),
        Some("Linux"),
        None,
        Some(selinux_mode),
        &[],
    )
}

// Rootless podman on an SELinux-enforcing host — the combination Fedora and
// openSUSE ship by default, and the one where the Docker-only probe answers
// nothing at all.
#[cfg(unix)]
fn run_wrapper_with_fake_podman(
    args: &[&str],
    rootless: bool,
    selinux: bool,
    selinux_mode: &str,
) -> String {
    run_wrapper_with_fake_docker_env(
        args,
        FakeInfo::Podman { rootless, selinux },
        Some("Linux"),
        None,
        Some(selinux_mode),
        &[],
    )
}

#[cfg(unix)]
fn run_wrapper_with_fake_docker_env(
    args: &[&str],
    fake_info: FakeInfo<'_>,
    uname_stdout: Option<&str>,
    claude_config_dir: Option<&str>,
    selinux_mode: Option<&str>,
    forwarded_env: &[(&str, &str)],
) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let docker_args = tmp.path().join("docker-args.txt");
    let docker = tmp.path().join("docker");
    let uname = tmp.path().join("uname");
    let id = tmp.path().join("id");
    let getenforce = tmp.path().join("getenforce");
    let info_branch = match fake_info {
        FakeInfo::Docker(stdout) => format!("  printf '%s\\n' '{stdout}'\n  exit 0\n"),
        FakeInfo::Podman { rootless, selinux } => format!(
            "  case \"$*\" in\n\
            \x20   *Host.Security.Rootless*) printf '{rootless}\\n' ; exit 0 ;;\n\
            \x20   *Host.Security.SELinuxEnabled*) printf '{selinux}\\n' ; exit 0 ;;\n\
            \x20 esac\n\
            \x20 printf '%s\\n' \"Error: template: info:1:2: executing \\\"info\\\" at \
             <.SecurityOptions>: can't evaluate field SecurityOptions in \
             type system.infoReport\" >&2\n\
            \x20 exit 125\n"
        ),
    };
    std::fs::write(
        &docker,
        format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = info ]; then\n{}fi\n\
             if [ \"$1\" = run ]; then\n  shift\n  printf '%s\\n' \"$@\" > {}\n  exit 0\nfi\n\
             exit 0\n",
            info_branch,
            shell_path(&docker_args)
        ),
    )
    .unwrap();
    if let Some(uname_stdout) = uname_stdout {
        std::fs::write(
            &uname,
            format!("#!/usr/bin/env bash\nprintf '{}\\n'\n", uname_stdout),
        )
        .unwrap();
    }
    std::fs::write(
        &id,
        "#!/usr/bin/env bash\n\
         case \"$1\" in\n\
           -u) printf '1000\\n' ;;\n\
           -g) printf '1000\\n' ;;\n\
           *) printf 'uid=1000 gid=1000 groups=1000\\n' ;;\n\
         esac\n",
    )
    .unwrap();
    std::fs::write(
        &getenforce,
        format!(
            "#!/usr/bin/env bash\nprintf '{}\\n'\n",
            selinux_mode.unwrap_or("Disabled")
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        if uname_stdout.is_some() {
            std::fs::set_permissions(&uname, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::set_permissions(&id, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&getenforce, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Always prepend the fake-binary dir to PATH: `id` is shadowed
    // unconditionally (see comment above), so PATH must always change, even
    // when `uname_stdout` is None and only `docker`/`id` are shadowed.
    let path = format!(
        "{}:{}",
        shell_path(tmp.path()),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = shell_script_command(&repo_root().join("bin/ai-memory"));
    command
        .args(args)
        .env("PATH", path)
        .env("AI_MEMORY_DOCKER", shell_path(&docker))
        .env("AI_MEMORY_NO_VERSION_CHECK", "1")
        .env("AI_MEMORY_DATA_VOLUME", "test-ai-memory-data")
        .env("HOME", shell_path(tmp.path()))
        .env_remove("AI_MEMORY_SERVER_URL")
        .env_remove("CLAUDE_CONFIG_DIR");
    if let Some(claude_config_dir) = claude_config_dir {
        command.env("CLAUDE_CONFIG_DIR", claude_config_dir);
    }
    for (name, value) in forwarded_env {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "wrapper failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(docker_args).unwrap()
}

#[cfg(unix)]
#[test]
fn wrapper_forwards_claude_config_dir_to_helper_container() {
    let args = run_wrapper_with_fake_docker_and_claude_config(
        &["install-hooks", "--agent", "claude-code", "--apply"],
        "[name=seccomp,profile=default]",
        "/home/alice/.config/claude",
    );
    assert!(
        args.contains("-e\nCLAUDE_CONFIG_DIR"),
        "wrapper must forward Claude's config root; got {args}"
    );
}

#[cfg(unix)]
fn run_wrapper_with_fake_rootless_docker_on_fake_macos(args: &[&str]) -> String {
    run_wrapper_with_fake_docker_and_uname(
        args,
        "[name=apparmor name=seccomp,profile=default name=rootless]",
        Some("Darwin"),
    )
}

#[cfg(unix)]
#[test]
fn rootless_docker_uses_root_uid_only_for_host_config_commands() {
    let rootless_info = "[name=apparmor name=seccomp,profile=default name=rootless]";

    for subcommand in [
        "install-mcp",
        "install-hooks",
        "setup-agent",
        "install-instructions",
        "install-skills",
        // uninstall edits the same host agent-config files; backup writes
        // its tarball to a host path, and restore reads one — same bind
        // mounts, same UID rule.
        "uninstall",
        "backup",
        "restore",
    ] {
        let args = run_wrapper_with_fake_docker(&[subcommand], rootless_info);
        assert!(
            args.contains("-u\n0:0"),
            "{subcommand} writes host bind-mounted files and must run as root \
             under rootless Docker so the write lands as the real host user \
             (rootlesskit only maps container UID 0 back to it); got {args}"
        );
    }

    let args = run_wrapper_with_fake_docker(&["status"], rootless_info);
    assert!(
        !args.contains("-u\n0:0"),
        "thin-client commands only touch the /data named volume, which isn't \
         host-visible, so they must keep the host-UID mapping; got {args}"
    );
}

#[cfg(unix)]
#[test]
fn fake_macos_rootless_docker_keeps_root_uid_for_host_config_commands() {
    let args = run_wrapper_with_fake_rootless_docker_on_fake_macos(&["install-mcp"]);
    assert!(
        args.contains("-u\n0:0"),
        "macOS rootless Docker still needs uid 0 for host config writes; got {args}"
    );

    let args = run_wrapper_with_fake_rootless_docker_on_fake_macos(&["status"]);
    assert!(
        !args.contains("-u\n0:0"),
        "macOS thin-client commands should keep Docker Desktop's default uid; got {args}"
    );
}

#[cfg(unix)]
#[test]
fn fake_macos_rootful_docker_keeps_default_uid_for_host_config_commands() {
    let args = run_wrapper_with_fake_docker_and_uname(
        &["install-mcp"],
        "[name=seccomp,profile=default]",
        Some("Darwin"),
    );
    assert!(
        !args.contains("-u\n0:0") && !args.contains("-u\n"),
        "macOS rootful Docker should keep Docker Desktop's default uid; got {args}"
    );
}

#[cfg(unix)]
#[test]
fn rootful_docker_keeps_host_uid_for_host_config_commands() {
    let rootful_info = "[name=seccomp,profile=default]";

    let args = run_wrapper_with_fake_docker(&["install-hooks"], rootful_info);
    assert!(
        !args.contains("-u\n0:0"),
        "rootful Docker must not switch to root UID — that would write \
         ~/.local/share/ai-memory/hooks owned by root instead of the invoking \
         user; got {args}"
    );
}

#[cfg(unix)]
#[test]
fn selinux_enforcing_disables_labels_only_for_host_file_commands() {
    let selinux_info = "[name=seccomp,profile=default name=selinux name=cgroupns]";

    for subcommand in [
        "install-mcp",
        "install-hooks",
        "setup-agent",
        "install-instructions",
        "install-skills",
        "uninstall",
        "backup",
        "restore",
    ] {
        let args = run_wrapper_with_fake_selinux(&[subcommand], selinux_info, "Enforcing");
        assert!(
            args.contains("--security-opt\nlabel=disable"),
            "{subcommand} writes bind-mounted host files and needs the scoped \
             SELinux exception; got {args}"
        );
    }

    let args = run_wrapper_with_fake_selinux(&["status"], selinux_info, "Enforcing");
    assert!(
        !args.contains("label=disable"),
        "thin-client commands must retain SELinux label confinement; got {args}"
    );
}

#[cfg(unix)]
#[test]
fn selinux_label_exception_requires_enforcement_and_daemon_support() {
    let selinux_info = "[name=seccomp,profile=default name=selinux name=cgroupns]";
    let args = run_wrapper_with_fake_selinux(&["install-mcp"], selinux_info, "Permissive");
    assert!(
        !args.contains("label=disable"),
        "permissive hosts do not need a label exception; got {args}"
    );

    let args = run_wrapper_with_fake_selinux(
        &["install-mcp"],
        "[name=seccomp,profile=default name=cgroupns]",
        "Enforcing",
    );
    assert!(
        !args.contains("label=disable"),
        "a daemon without SELinux support must not receive SELinux options; got {args}"
    );
}

#[cfg(unix)]
#[test]
fn posix_wrapper_forwards_subscription_oauth_tokens_without_putting_values_in_argv() {
    let tokens = [
        ("ANTHROPIC_OAUTH_TOKEN", "oauth-canary-primary"),
        ("CLAUDE_CODE_OAUTH_TOKEN", "oauth-canary-fallback"),
    ];
    let args = run_wrapper_with_fake_docker_and_forwarded_env(
        &["llm-test", "--provider", "anthropic-oauth"],
        "[name=seccomp,profile=default]",
        &tokens,
    );
    let args: Vec<&str> = args.lines().collect();

    for (name, value) in tokens {
        assert!(
            args.windows(2).any(|pair| pair == ["-e", name]),
            "wrapper must forward {name} by name; got {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg.contains(value)),
            "wrapper must not put the value of {name} in Docker argv"
        );
    }
}

#[test]
fn powershell_wrapper_lists_subscription_oauth_tokens_in_its_env_allowlist() {
    let wrapper = read_repo("bin/ai-memory.ps1");
    let allowlist = wrapper
        .split_once("foreach ($Name in @(")
        .and_then(|(_, rest)| rest.split_once(")) {"))
        .map(|(allowlist, _)| allowlist)
        .expect("PowerShell wrapper env allowlist");

    for name in ["ANTHROPIC_OAUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"] {
        assert!(
            allowlist
                .lines()
                .any(|line| line.trim() == format!("\"{name}\",")),
            "PowerShell wrapper must list {name} in the helper env allowlist"
        );
    }
}

#[test]
fn powershell_wrapper_trims_paths_with_single_character_separators() {
    let wrapper = read_repo("bin/ai-memory.ps1");

    assert_eq!(
        wrapper.matches(r"TrimEnd([char[]]@('/', '\'))").count(),
        2,
        "PowerShell TrimEnd arguments must contain individual characters"
    );
    assert!(
        !wrapper.contains(r"TrimEnd([char[]]@('/', '\\'))"),
        "a multi-character backslash string cannot be converted to System.Char"
    );
}

// The PowerShell counterpart of run_wrapper_on_fake_macos: a fake `docker.cmd`
// records the argv the wrapper built. No `uname` shadowing is needed because
// the Windows arm under test is selected by running on Windows at all.
#[cfg(windows)]
fn run_powershell_wrapper(args: &[&str]) -> String {
    run_powershell_wrapper_with_server_url(args, None)
}

#[cfg(windows)]
fn run_powershell_wrapper_with_server_url(args: &[&str], server_url: Option<&str>) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let docker_args = tmp.path().join("docker-args.txt");
    let docker = tmp.path().join("docker.cmd");
    std::fs::write(
        &docker,
        "@echo off\r\n>\"%AI_MEMORY_TEST_DOCKER_ARGS%\" echo %*\r\nexit /b 0\r\n",
    )
    .unwrap();

    let mut command = Command::new("powershell.exe");
    // See the execution-policy note on the OAuth-token test below.
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(repo_root().join("bin/ai-memory.ps1"))
        .args(args)
        .env("AI_MEMORY_DOCKER", &docker)
        .env("AI_MEMORY_TEST_DOCKER_ARGS", &docker_args)
        .env("AI_MEMORY_DATA_VOLUME", "test-ai-memory-data")
        .env_remove("AI_MEMORY_DATA_DIR");
    match server_url {
        Some(url) => command.env("AI_MEMORY_SERVER_URL", url),
        None => command.env_remove("AI_MEMORY_SERVER_URL"),
    };
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "PowerShell wrapper failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::read_to_string(docker_args).unwrap()
}

// The Windows mirror of macos_wrapper_routes_urls_by_real_subcommand: Docker
// Desktop gives Linux containers no host networking on Windows either, so the
// helper container cannot reach the host-published server over loopback.
#[cfg(windows)]
#[test]
fn windows_wrapper_routes_urls_by_real_subcommand() {
    for subcommand in ["install-mcp", "install-hooks", "setup-agent"] {
        let args = run_powershell_wrapper(&[subcommand]);
        assert!(
            !args.contains("AI_MEMORY_SERVER_URL=http://host.docker.internal:49374"),
            "{subcommand} renders host-side config and must keep loopback defaults; got {args}"
        );
    }

    let args = run_powershell_wrapper(&["status"]);
    assert!(
        args.contains("AI_MEMORY_SERVER_URL=http://host.docker.internal:49374"),
        "thin-client commands must reach the host server through Docker Desktop; got {args}"
    );

    let args = run_powershell_wrapper(&["search", "install-hooks"]);
    assert!(
        args.contains("AI_MEMORY_SERVER_URL=http://host.docker.internal:49374"),
        "only the actual subcommand should control URL routing; got {args}"
    );

    let args = run_powershell_wrapper(&["--config", "C:\\tmp\\config.toml", "install-hooks"]);
    assert!(
        !args.contains("AI_MEMORY_SERVER_URL=http://host.docker.internal:49374"),
        "global options before install-hooks must not hide the real subcommand; got {args}"
    );

    // A homelab/remote server is configured through the environment, and the
    // helper container must not be redirected back at the local Docker host.
    let args =
        run_powershell_wrapper_with_server_url(&["status"], Some("http://192.168.1.50:49374"));
    assert!(
        !args.contains("AI_MEMORY_SERVER_URL=http://host.docker.internal:49374"),
        "an explicit AI_MEMORY_SERVER_URL must win over the Docker Desktop alias; got {args}"
    );
    assert!(
        args.contains("-e AI_MEMORY_SERVER_URL"),
        "an explicit AI_MEMORY_SERVER_URL must still be forwarded by name; got {args}"
    );
}

#[cfg(windows)]
#[test]
fn powershell_wrapper_forwards_subscription_oauth_tokens_without_putting_values_in_argv() {
    let tmp = tempfile::tempdir().unwrap();
    let docker_args = tmp.path().join("docker-args.txt");
    let docker = tmp.path().join("docker.cmd");
    std::fs::write(
        &docker,
        "@echo off\r\n>\"%AI_MEMORY_TEST_DOCKER_ARGS%\" echo %*\r\nexit /b 0\r\n",
    )
    .unwrap();

    let tokens = [
        ("ANTHROPIC_OAUTH_TOKEN", "oauth-canary-primary"),
        ("CLAUDE_CODE_OAUTH_TOKEN", "oauth-canary-fallback"),
    ];
    let mut command = Command::new("powershell.exe");
    command
        // -ExecutionPolicy Bypass: `-File` loads a script from disk, and execution policy
        // governs script *files*, so on a machine left at the Windows client default of
        // `Restricted` the unsigned bin/ai-memory.ps1 is refused (`UnauthorizedAccess`) and
        // the wrapper never runs — failing this test for a reason unrelated to what it
        // checks. The other in-repo invocation (render_shared.rs) uses `-Command`, which is
        // not policy-gated and therefore needs no override; the generated hook commands pass
        // Bypass defensively. GitHub's runner is permissive enough that this passes there
        // today, so the fix is for running the suite on a stock Windows install.
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(repo_root().join("bin/ai-memory.ps1"))
        .args(["llm-test", "--provider", "anthropic-oauth"])
        .env("AI_MEMORY_DOCKER", &docker)
        .env("AI_MEMORY_TEST_DOCKER_ARGS", &docker_args);
    for (name, value) in tokens {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "PowerShell wrapper failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let args = std::fs::read_to_string(docker_args).unwrap();
    for (name, value) in tokens {
        assert!(
            args.contains(&format!("-e {name}")),
            "PowerShell wrapper must forward {name} by name; got {args}"
        );
        assert!(
            !args.contains(value),
            "PowerShell wrapper must not put the value of {name} in Docker argv"
        );
    }
}

#[cfg(unix)]
#[test]
fn podman_rootless_and_selinux_are_detected_without_the_docker_only_field() {
    // Podman answers nothing for `{{.SecurityOptions}}`, so both gates used to
    // read as "rootful, no SELinux" and every host-file write died with
    // Permission denied. Both adjustments are required: neither alone makes
    // the write land.
    for subcommand in [
        "install-mcp",
        "install-hooks",
        "setup-agent",
        "install-instructions",
        "install-skills",
        "uninstall",
        "backup",
        "restore",
    ] {
        let args = run_wrapper_with_fake_podman(&[subcommand], true, true, "Enforcing");
        assert!(
            args.contains("-u\n0:0"),
            "{subcommand} needs the rootless UID remap under podman too; got {args}"
        );
        assert!(
            args.contains("--security-opt\nlabel=disable"),
            "{subcommand} needs the scoped SELinux exception under podman too; got {args}"
        );
    }

    let args = run_wrapper_with_fake_podman(&["status"], true, true, "Enforcing");
    assert!(
        !args.contains("-u\n0:0") && !args.contains("label=disable"),
        "thin-client commands touch only the named volume and must stay \
         confined under podman as well; got {args}"
    );
}

#[cfg(unix)]
#[test]
fn podman_gates_still_respect_engine_and_host_state() {
    // The fallback reports what podman reports; it must not hard-code "yes".
    let args = run_wrapper_with_fake_podman(&["install-mcp"], false, true, "Enforcing");
    assert!(
        !args.contains("-u\n0:0"),
        "rootful podman maps the host UID directly and must keep it; got {args}"
    );

    let args = run_wrapper_with_fake_podman(&["install-mcp"], true, false, "Enforcing");
    assert!(
        !args.contains("label=disable"),
        "an engine without SELinux support must not receive SELinux options; got {args}"
    );

    let args = run_wrapper_with_fake_podman(&["install-mcp"], true, true, "Permissive");
    assert!(
        !args.contains("label=disable"),
        "permissive hosts do not need a label exception; got {args}"
    );
}

#[cfg(unix)]
#[test]
fn bootstrap_gets_host_file_treatment_because_it_reads_the_repo() {
    // bootstrap only reads host files, but an unmapped UID blocks reads just
    // as hard: it degrades silently to "no .git found at /work" and then dies
    // with Permission denied. Same gates as the writers, on both engines.
    let args = run_wrapper_with_fake_podman(&["bootstrap"], true, true, "Enforcing");
    assert!(
        args.contains("-u\n0:0") && args.contains("--security-opt\nlabel=disable"),
        "bootstrap reads the repo bind-mounted at /work and needs both \
         adjustments; got {args}"
    );

    let args = run_wrapper_with_fake_selinux(
        &["bootstrap"],
        "[name=seccomp,profile=default name=selinux name=cgroupns]",
        "Enforcing",
    );
    assert!(
        args.contains("--security-opt\nlabel=disable"),
        "the same read applies under Docker on an SELinux host; got {args}"
    );
}

#[cfg(unix)]
#[test]
fn explicit_config_gets_host_file_treatment() {
    let args = run_wrapper_with_fake_podman(
        &["--config", "/tmp/config.toml", "status"],
        true,
        true,
        "Enforcing",
    );
    assert!(
        args.contains("-u\n0:0") && args.contains("--security-opt\nlabel=disable"),
        "an explicit config is read through a host bind and needs both adjustments; got {args}"
    );

    let args = run_wrapper_with_fake_podman(
        &["--config=/tmp/config.toml", "status"],
        true,
        true,
        "Enforcing",
    );
    assert!(
        args.contains("-u\n0:0") && args.contains("--security-opt\nlabel=disable"),
        "the equals form of --config must receive the same treatment; got {args}"
    );
}

#[cfg(unix)]
#[test]
fn custom_data_dir_makes_thin_commands_touch_host_files() {
    let args = run_wrapper_with_fake_docker_env(
        &["status"],
        FakeInfo::Podman {
            rootless: true,
            selinux: true,
        },
        Some("Linux"),
        None,
        Some("Enforcing"),
        &[("AI_MEMORY_DATA_DIR", "/tmp")],
    );
    assert!(
        args.contains("-u\n0:0") && args.contains("--security-opt\nlabel=disable"),
        "a thin command backed by a host data directory needs both adjustments; got {args}"
    );
    assert!(
        args.contains("/tmp:/data"),
        "the custom data directory must remain the /data bind; got {args}"
    );
}

#[test]
fn macos_docs_use_valid_install_commands_and_release_body_points_to_them() {
    let docs = read_repo("docs/macos.md");
    assert!(docs.contains("install-hooks --agent claude-code --apply"));
    assert!(docs.contains("install-mcp --client claude-code --apply"));
    assert!(
        !docs.contains("setup-agent --agent claude-code --source ./hooks"),
        "setup-agent has no --apply path; use install-hooks for native macOS docs"
    );
    assert!(
        !docs.contains("init` configures the bearer token"),
        "init writes token_pepper, not a bearer token"
    );
    assert!(docs.contains("Host-side agent config should use"));
    assert!(docs.contains("Tagged releases publish a multi-arch manifest"));

    let release = read_repo(".github/workflows/release.yml");
    assert!(release.contains("follow the bundled docs/macos.md"));
}
