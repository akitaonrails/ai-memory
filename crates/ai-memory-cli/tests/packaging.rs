//! Packaging asset regression tests.

#[cfg(unix)]
use std::io::BufRead as _;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::process::Stdio;

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
}

// Unix-only alongside run_wrapper_on_fake_macos below — these helpers'
// former Git Bash arms existed to run the wrapper test on Windows, which
// the fake-uname executable-bit limitation rules out anyway.
#[cfg(unix)]
fn shell_script_command(script: &Path) -> Command {
    Command::new(script)
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
    assert!(release.contains("needs: [binary, windows, validate-version]")); // macoos
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

#[cfg(unix)]
#[test]
fn managed_run_wrapper_uses_host_binary_path_and_remote_server_without_docker() {
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
    let output = shell_script_command(&repo_root().join("bin/ai-memory"))
        .args(["run", "codex", "--yolo", "resume"])
        .env("AI_MEMORY_NATIVE_BIN", &native)
        .env("AI_MEMORY_DOCKER", &docker)
        .env("AI_MEMORY_SERVER_URL", "http://192.168.0.90:49374")
        .env("AI_MEMORY_AUTH_TOKEN", "remote-test-token")
        .env("PATH", &host_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "wrapper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record = std::fs::read_to_string(record).unwrap();
    assert_eq!(
        record,
        format!(
            "server=http://192.168.0.90:49374\n\
             auth=remote-test-token\n\
             path={host_path}\n\
             arg=run\n\
             arg=codex\n\
             arg=--yolo\n\
             arg=resume\n"
        )
    );
    assert!(!docker_record.exists(), "managed run entered Docker");
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
        docker_info_stdout,
        None,
        Some(claude_config_dir),
        None,
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
    run_wrapper_with_fake_docker_env(args, docker_info_stdout, uname_stdout, None, None)
}

#[cfg(unix)]
fn run_wrapper_with_fake_selinux(
    args: &[&str],
    docker_info_stdout: &str,
    selinux_mode: &str,
) -> String {
    run_wrapper_with_fake_docker_env(
        args,
        docker_info_stdout,
        Some("Linux"),
        None,
        Some(selinux_mode),
    )
}

#[cfg(unix)]
fn run_wrapper_with_fake_docker_env(
    args: &[&str],
    docker_info_stdout: &str,
    uname_stdout: Option<&str>,
    claude_config_dir: Option<&str>,
    selinux_mode: Option<&str>,
) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let docker_args = tmp.path().join("docker-args.txt");
    let docker = tmp.path().join("docker");
    let uname = tmp.path().join("uname");
    let id = tmp.path().join("id");
    let getenforce = tmp.path().join("getenforce");
    std::fs::write(
        &docker,
        format!(
            "#!/usr/bin/env bash\n\
             if [ \"$1\" = info ]; then\n  printf '%s\\n' '{}'\n  exit 0\nfi\n\
             if [ \"$1\" = run ]; then\n  shift\n  printf '%s\\n' \"$@\" > {}\n  exit 0\nfi\n\
             exit 0\n",
            docker_info_stdout,
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
        // its tarball to a host path — same bind mounts, same UID rule.
        "uninstall",
        "backup",
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
