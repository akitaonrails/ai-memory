//! End-to-end coverage for `ai-memory run --jail[=…]` / `--no-jail` through
//! the built binary (docs/design-yolo-safety-ai-jail.md §5).
//!
//! Each test puts fake `ai-jail`, sandbox-backend (`bwrap` on Linux,
//! `sandbox-exec` on macOS), and `claude` executables on a temp
//! `PATH`, runs the real `ai-memory run` in a temp git repository with a temp
//! `HOME`, and points it at a local mock server that records every request.
//! The fake `ai-jail` answers `--help` with a chosen help text (so support
//! detection sees either a 2.4.1- or a 2.5.0-style binary) and otherwise
//! writes its argv, one argument per line, to a file — the exact invocation
//! `ai-memory` exec'd. Unix-only: the fakes are shebang scripts and the
//! re-exec is a real `exec`.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};

use ai_memory_workstream::inside_ai_jail_here;
use axum::{Json, Router, http::StatusCode, routing::post};
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");

/// Toggle lines of ai-jail 2.4.1's `--help`: no credential mounts.
const HELP_2_4_1: &str = "\
    --no-gpu / --gpu               Disable/enable GPU device passthrough (Linux only)
    --no-docker / --docker         Disable/enable Docker socket passthrough (grants host root; default: off)
    --tailscale / --no-tailscale   Enable/disable Tailscale socket passthrough (default: off)
    --no-display / --display       Disable/enable X11/Wayland passthrough (Linux only)
    --network / --no-network       Enable/disable unrestricted network access (default: off)
    --agent-state / --no-agent-state
    --save-config / --no-save-config
    --worktree / --no-worktree     Enable/disable linked Git worktree metadata passthrough
    --no-mise / --mise             Disable/enable mise integration
    --ssh / --no-ssh               Share ~/.ssh read-only + forward SSH_AUTH_SOCK (default: off)
    --pictures / --no-pictures     Share ~/Pictures read-only (default: off)
";

/// What ai-jail 2.5.0 adds.
const HELP_2_5_0_EXTRA: &str = "\
    --github / --no-github         Enable/disable read-only ~/.config/gh mount
    --aws / --no-aws               Enable/disable read-only ~/.aws mount
    --kube / --no-kube             Enable/disable read-only ~/.kube mount
    --gcloud / --no-gcloud         Enable/disable read-only ~/.config/gcloud
    --docker-config / --no-docker-config
    --no-toolchains / --toolchains Disable/enable dev-toolchain cache
";

/// What ai-jail 2.15 adds that these tests use: folder mounts.
const HELP_MAPS: &str = "\
    --map <PATH|SOURCE:DEST>       Mount read-only, optionally at DEST
    --rw-map <PATH|SOURCE:DEST>    Mount read-write, optionally at DEST
";

struct Fixture {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    jail_argv: PathBuf,
    /// `AI_MEMORY_JAIL_NEXT_RUN` as the fake ai-jail received it.
    next_run: PathBuf,
    claude_ran: PathBuf,
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn host_tool(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH");
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} on PATH"))
}

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

impl Fixture {
    /// `help` is what the fake `ai-jail --help` prints; `with_backend` decides
    /// whether ai-jail counts as usable; `origin` is the repo's remote.
    fn new(help: &str, with_backend: bool, origin: Option<&str>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        let home = root.join("home");
        let bin = root.join("bin");
        for dir in [&repo, &home, &bin] {
            fs::create_dir_all(dir).unwrap();
        }
        git(&repo, &["init", "-q"]);
        if let Some(origin) = origin {
            git(&repo, &["remote", "add", "origin", origin]);
        }
        let help_file = root.join("ai-jail-help.txt");
        fs::write(&help_file, help).unwrap();
        let jail_argv = root.join("ai-jail-argv.txt");
        let next_run = root.join("ai-jail-next-run.txt");
        let claude_ran = root.join("claude-ran.txt");
        // Sibling projects to mount, next to the repository.
        for sibling in ["ai-jail", "tap"] {
            fs::create_dir_all(root.join(sibling)).unwrap();
        }
        // `--help` feeds toggle support; `--dry-run` is the usable-ai-jail
        // preflight (`usable_ai_jail_here`). Neither is a real re-exec, so
        // neither may touch `jail_argv` — tests assert its absence when the
        // wrapper stays unjailed or rejects before exec.
        write_script(
            &bin.join("ai-jail"),
            &format!(
                "if [ \"$1\" = --help ]; then cat '{}'; exit 0; fi\n\
                 if [ \"$1\" = --dry-run ]; then exit 0; fi\n\
                 printf '%s\\n' \"$@\" > '{}'\n\
                 printf '%s' \"$AI_MEMORY_JAIL_NEXT_RUN\" > '{}'\n",
                help_file.display(),
                jail_argv.display(),
                next_run.display()
            ),
        );
        if with_backend {
            // The backend `usable_ai_jail` requires on this OS: a fake `bwrap`
            // on Linux would leave ai-jail unusable on macOS, where it needs
            // `sandbox-exec`.
            let backend = if cfg!(target_os = "macos") {
                "sandbox-exec"
            } else {
                "bwrap"
            };
            write_script(&bin.join(backend), "exit 0\n");
        }
        // `PATH` is this directory alone, so a host sandbox backend cannot make
        // ai-jail usable behind the test's back; link in the two host tools
        // the run itself needs.
        for tool in ["git", "cat"] {
            std::os::unix::fs::symlink(host_tool(tool), bin.join(tool)).unwrap();
        }
        // The fake harness also writes the (empty-session) transcript the
        // launcher waits for, so an unjailed run finishes at once instead of
        // waiting out the transcript-flush poll.
        let transcripts = home
            .join(".claude/projects")
            .join(repo.to_string_lossy().replace('/', "-"));
        fs::create_dir_all(&transcripts).unwrap();
        write_script(
            &bin.join("claude"),
            &format!(
                "printf '%s\\n' \"$@\" > '{ran}'\n\
                 while [ $# -gt 0 ]; do\n\
                 if [ \"$1\" = --session-id ]; then\n\
                 printf '{{\"sessionId\":\"%s\",\"cwd\":\"%s\"}}\\n' \"$2\" '{repo}' > '{dir}'/\"$2\".jsonl\n\
                 fi\n\
                 shift\n\
                 done\n",
                ran = claude_ran.display(),
                repo = repo.display(),
                dir = transcripts.display(),
            ),
        );
        Self {
            _temp: temp,
            repo,
            home,
            bin,
            jail_argv,
            next_run,
            claude_ran,
        }
    }

    /// A sibling of the repository, created by [`Fixture::new`].
    fn sibling(&self, name: &str) -> PathBuf {
        self.repo.parent().unwrap().join(name)
    }

    /// The `(flag, path)` folder mounts before the `--`.
    fn mounts(&self) -> Vec<(String, PathBuf)> {
        let argv = self.jail_argv().expect("ai-jail exec'd");
        let (flags, _) = split_at_separator(&argv);
        flags
            .windows(2)
            .filter(|pair| pair[0] == "--map" || pair[0] == "--rw-map")
            .map(|pair| (pair[0].clone(), PathBuf::from(&pair[1])))
            .collect()
    }

    fn jail_argv(&self) -> Option<Vec<String>> {
        fs::read_to_string(&self.jail_argv)
            .ok()
            .map(|text| text.lines().map(str::to_owned).collect())
    }
}

/// A mock ai-memory server: `POST /workstream/runs` opens a run, `…/finish`
/// acknowledges it, everything else answers 204. Every request path is
/// recorded.
async fn mock_server() -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let app = Router::new().fallback(post(move |uri: axum::http::Uri| {
        let observed = observed.clone();
        async move {
            observed.lock().unwrap().push(uri.path().to_owned());
            if uri.path() == "/workstream/recent" {
                (
                    StatusCode::OK,
                    Json(json!([{
                        "workstream_id": "12345678-1234-4234-9234-123456789abd",
                        "name": "fixture", "current": true,
                        "created_at": "2026-09-01T00:00:00Z",
                        "last_active_at": "2026-09-01T00:00:00Z",
                        "linked_harnesses": ["claude-code"]
                    }])),
                )
            } else if uri.path() == "/workstream/runs" {
                (
                    StatusCode::OK,
                    Json(json!({
                        "workstream_id": "12345678-1234-4234-9234-123456789abd",
                        "workstream_name": "fixture",
                        "run_id": "12345678-1234-4234-9234-123456789abe",
                        "resolved_agent": "claude-code",
                        "sync_after": 0, "sync_through": 0,
                        "may_adopt_existing_session": false,
                    })),
                )
            } else if uri.path().ends_with("/finish") {
                (
                    StatusCode::OK,
                    Json(json!({"imported_events": 0, "latest_sequence": 0})),
                )
            } else {
                (StatusCode::NO_CONTENT, Json(json!(null)))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{address}"), requests, server)
}

fn command(fixture: &Fixture, server: &str, args: &[&str]) -> tokio::process::Command {
    let mut command: tokio::process::Command = crate::e2e_support::hermetic(BIN).into();
    for name in ["SSH_AUTH_SOCK", "XDG_CONFIG_HOME", "GH_CONFIG_DIR", "PS1"] {
        command.env_remove(name);
    }
    command
        .args(args)
        .current_dir(&fixture.repo)
        .env("PATH", &fixture.bin)
        .env("HOME", &fixture.home)
        .env("AI_MEMORY_HOME", &fixture.home)
        .env("CLAUDE_CONFIG_DIR", fixture.home.join(".claude"))
        .env("AI_MEMORY_DATA_DIR", fixture.home.join("data"))
        .env("AI_MEMORY_SERVER_URL", server)
        .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

async fn run(fixture: &Fixture, server: &str, args: &[&str]) -> Output {
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        command(fixture, server, args).output(),
    )
    .await
    .expect("ai-memory run finished")
    .expect("spawn ai-memory")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Split the recorded ai-jail argv at `--`: (sandbox flags, wrapped command).
fn split_at_separator(argv: &[String]) -> (&[String], &[String]) {
    let separator = argv
        .iter()
        .position(|arg| arg == "--")
        .expect("`--` separates the sandbox flags from the wrapped command");
    (&argv[..separator], &argv[separator + 1..])
}

/// The toggles in the sandbox flags: everything after the `--network
/// --agent-state --no-save-config` baseline and its `--env NAME` pairs. The
/// baseline is asserted on every exec: without `--no-save-config` ai-jail
/// would write these flags into the repository's `.ai-jail`.
fn toggles(flags: &[String]) -> Vec<String> {
    assert_eq!(
        &flags[..3],
        ["--network", "--agent-state", "--no-save-config"],
        "{flags:?}"
    );
    let mut rest = &flags[3..];
    while rest.first().is_some_and(|flag| flag == "--env") {
        rest = &rest[2..];
    }
    rest.to_vec()
}

/// The checklist rows an empty fixture `$HOME` shows with the 2.4.1 help:
/// only the opt-in capabilities (GPU and display are Linux-only).
fn visible_capability_rows() -> Vec<&'static str> {
    if cfg!(target_os = "linux") {
        vec!["docker", "gpu", "display", "pictures", "tailscale"]
    } else {
        vec!["docker", "pictures", "tailscale"]
    }
}

/// What an explicit selection emits: the named flags in order, then `--no-X`
/// for every visible checklist row the selection did not mention.
fn exact(named: &[&str]) -> Vec<String> {
    let mut expected: Vec<String> = named.iter().map(|flag| (*flag).to_owned()).collect();
    for row in visible_capability_rows() {
        let mentioned = named
            .iter()
            .any(|flag| *flag == format!("--{row}") || *flag == format!("--no-{row}"));
        if !mentioned {
            expected.push(format!("--no-{row}"));
        }
    }
    expected
}

fn exe() -> String {
    Path::new(BIN)
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// A sandboxed test run would see itself as already jailed, where every jail
/// flag is (correctly) ignored.
fn skip_inside_ai_jail() -> bool {
    let jailed = inside_ai_jail_here();
    if jailed {
        eprintln!("skipping: already inside ai-jail, where --jail is ignored by design");
    }
    jailed
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jail_list_reexecs_under_ai_jail_with_exactly_the_listed_toggles() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    let (server, requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &["run", "--no-autowire", "claude", "--jail=gpu,ssh"],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let argv = fixture
        .jail_argv()
        .unwrap_or_else(|| panic!("ai-jail was never exec'd:\n{}", stderr(&output)));
    let (flags, wrapped) = split_at_separator(&argv);
    assert!(
        flags
            .windows(2)
            .any(|pair| pair == ["--env", "AI_MEMORY_SERVER_URL"]),
        "the server URL is forwarded into the jail: {flags:?}"
    );
    assert_eq!(
        toggles(flags),
        exact(&["--gpu", "--ssh"]),
        "exactly the list: every other visible row is forced off"
    );
    assert_eq!(
        wrapped,
        [
            exe().as_str(),
            "run",
            "--no-autowire",
            "claude",
            "--jail=gpu,ssh"
        ],
        "the original invocation is wrapped verbatim after `--`"
    );
    assert!(
        stderr(&output).contains("capabilities: GPU devices"),
        "{}",
        stderr(&output)
    );
    assert!(
        !fixture.claude_ran.exists(),
        "the harness never runs outside"
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "an explicit --jail re-execs before opening a managed run: {:?}",
        requests.lock().unwrap()
    );
    handle.abort();
}

/// Bare `--jail` takes the smart defaults: credentials present in `$HOME`
/// that the installed ai-jail supports, plus SSH for an SSH `origin`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_jail_uses_the_smart_defaults() {
    if skip_inside_ai_jail() {
        return;
    }
    let help = format!("{HELP_2_4_1}{HELP_2_5_0_EXTRA}");
    let fixture = Fixture::new(&help, true, Some("git@github.com:example/repo.git"));
    for dir in [".aws", ".ssh"] {
        fs::create_dir_all(fixture.home.join(dir)).unwrap();
    }
    let (server, requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &["run", "--jail", "--no-autowire", "claude"],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let argv = fixture.jail_argv().expect("ai-jail exec'd");
    let (flags, wrapped) = split_at_separator(&argv);
    assert_eq!(
        toggles(flags),
        ["--aws", "--ssh"],
        "absent credentials and opt-in capabilities stay off"
    );
    assert_eq!(wrapped[1..], ["run", "--jail", "--no-autowire", "claude"]);
    assert!(requests.lock().unwrap().is_empty());
    handle.abort();
}

/// A project `.ai-jail` that asks for dangerous things. Its contents are
/// irrelevant to ai-memory (never read); ai-jail applies its own trust rules.
const DANGEROUS_PROJECT_CONFIG: &str = "docker = true\ngithub = true\nagent_state = true\n";

/// Bare `--jail` with a project `.ai-jail` defers to the file: none of the
/// smart defaults this host would otherwise get (`--aws`, `--ssh`) and
/// nothing the file asks for (`--docker`) is passed — only the baseline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_jail_with_a_project_ai_jail_passes_no_toggles() {
    if skip_inside_ai_jail() {
        return;
    }
    let help = format!("{HELP_2_4_1}{HELP_2_5_0_EXTRA}");
    let fixture = Fixture::new(&help, true, Some("git@github.com:example/repo.git"));
    for dir in [".aws", ".ssh"] {
        fs::create_dir_all(fixture.home.join(dir)).unwrap();
    }
    fs::write(fixture.repo.join(".ai-jail"), DANGEROUS_PROJECT_CONFIG).unwrap();
    let (server, requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &["run", "--jail", "--no-autowire", "claude"],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let argv = fixture.jail_argv().expect("ai-jail exec'd");
    let (flags, wrapped) = split_at_separator(&argv);
    assert_eq!(toggles(flags), Vec::<String>::new());
    assert_eq!(
        wrapped,
        [exe().as_str(), "run", "--jail", "--no-autowire", "claude"]
    );
    assert!(
        stderr(&output).contains("plus the project .ai-jail"),
        "{}",
        stderr(&output)
    );
    assert!(requests.lock().unwrap().is_empty());
    handle.abort();
}

/// An explicit list still applies on top of a project `.ai-jail` (CLI flags
/// override config in ai-jail).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jail_list_overrides_a_project_ai_jail() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    fs::write(fixture.repo.join(".ai-jail"), DANGEROUS_PROJECT_CONFIG).unwrap();
    let (server, _requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &["run", "--jail=gpu,no-docker", "--no-autowire", "claude"],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let argv = fixture.jail_argv().expect("ai-jail exec'd");
    let (flags, _) = split_at_separator(&argv);
    assert_eq!(toggles(flags), exact(&["--gpu", "--no-docker"]));
    handle.abort();
}

/// `--jail` must never fall back to an unjailed run: no sandbox backend means
/// ai-jail is unusable, so the launch fails before anything starts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jail_fails_closed_when_ai_jail_is_not_usable() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(HELP_2_4_1, false, None);
    let (server, requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &["run", "--jail", "--no-autowire", "claude"],
    )
    .await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("ai-jail is not usable on this host"),
        "{}",
        stderr(&output)
    );
    assert!(fixture.jail_argv().is_none());
    assert!(!fixture.claude_ran.exists(), "never runs unjailed");
    assert!(requests.lock().unwrap().is_empty());
    handle.abort();
}

/// A toggle the installed ai-jail lacks is an error naming the release that
/// has it — never passed through for an older ai-jail to reject.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jail_rejects_a_toggle_the_installed_ai_jail_lacks() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    let (server, requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &["run", "--jail=gpu,github", "--no-autowire", "claude"],
    )
    .await;
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("needs ai-jail 2.5.0 or newer"),
        "{}",
        stderr(&output)
    );
    assert!(fixture.jail_argv().is_none());
    assert!(!fixture.claude_ran.exists());
    assert!(requests.lock().unwrap().is_empty());

    let reserved = run(
        &fixture,
        &server,
        &["run", "--jail=no-seccomp", "--no-autowire", "claude"],
    )
    .await;
    assert!(!reserved.status.success());
    assert!(
        stderr(&reserved).contains("not available through ai-memory; run ai-jail directly"),
        "{}",
        stderr(&reserved)
    );
    assert!(fixture.jail_argv().is_none());
    handle.abort();
}

/// `--no-jail` runs the harness directly even where ai-jail is usable, and
/// never hands the flag to the harness.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_jail_runs_the_harness_without_ai_jail() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    let (server, requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &[
            "run",
            "--no-autowire",
            "claude",
            "--model",
            "opus",
            "--no-jail",
        ],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        fixture.claude_ran.exists(),
        "the harness ran:\n{}",
        stderr(&output)
    );
    let harness_args = fs::read_to_string(&fixture.claude_ran).unwrap();
    assert!(
        !harness_args.contains("jail"),
        "--no-jail is a wrapper flag: {harness_args}"
    );
    assert!(fixture.jail_argv().is_none(), "ai-jail must not be invoked");
    assert_eq!(
        requests.lock().unwrap().first().map(String::as_str),
        Some("/workstream/runs"),
        "the unjailed run opens its managed run as usual"
    );
    handle.abort();
}

/// With a real terminal, `--yolo --no-jail` still shows the warning but never
/// the ai-jail offer. Without `--no-jail` the offer would read the closed
/// stdin as its default "yes" and exec the fake ai-jail, so this bites.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_no_jail_warns_without_offering_ai_jail_on_a_terminal() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    let (server, _requests, handle) = mock_server().await;
    let output = terminal_run(
        &fixture,
        &server,
        "run --no-autowire --yolo --no-jail claude",
        "y\n",
    )
    .await;
    assert!(
        output.contains("--yolo runs every tool call"),
        "the yolo warning still shows:\n{output}"
    );
    assert!(
        !output.contains("ai-jail is installed"),
        "--no-jail suppresses the offer:\n{output}"
    );
    assert!(fixture.jail_argv().is_none());
    assert!(fixture.claude_ran.exists(), "{output}");
    handle.abort();
}

/// The control for the test above: the same terminal run without `--no-jail`
/// is offered ai-jail, takes the default yes, and (no checklist rows on an
/// empty `$HOME` beyond the opt-in capabilities, accepted as marked) execs it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_offer_then_checklist_execs_ai_jail_on_a_terminal() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    let (server, requests, handle) = mock_server().await;
    let output = terminal_run(
        &fixture,
        &server,
        "run --no-autowire --yolo claude",
        "y\ny\n2\n\n",
    )
    .await;
    assert!(output.contains("ai-jail is installed"), "{output}");
    assert!(
        output.contains("ai-jail: what the agent can reach"),
        "{output}"
    );
    let argv = fixture
        .jail_argv()
        .unwrap_or_else(|| panic!("ai-jail exec'd:\n{output}"));
    let (flags, _) = split_at_separator(&argv);
    let expected: Vec<String> = visible_capability_rows()
        .iter()
        .enumerate()
        .map(|(index, row)| {
            if index == 1 {
                format!("--{row}")
            } else {
                format!("--no-{row}")
            }
        })
        .collect();
    assert_eq!(
        toggles(flags),
        expected,
        "row 2 was flipped on; every other row is passed as the user saw it (off)"
    );
    assert!(!fixture.claude_ran.exists());
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.as_slice(),
        [
            "/workstream/runs",
            "/workstream/runs/12345678-1234-4234-9234-123456789abe/cancel"
        ],
        "accepting the offer cancels the prepared run before the re-exec"
    );
    handle.abort();
}

/// With a project `.ai-jail` the offer re-execs straight away: no checklist
/// (a repository must not be able to steer it) and no toggles of ours, even
/// though the file asks for `docker`. The input holds a "2" that would flip
/// GPU on if a checklist were wrongly shown.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn yolo_offer_skips_the_checklist_for_a_project_ai_jail() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    fs::write(fixture.repo.join(".ai-jail"), DANGEROUS_PROJECT_CONFIG).unwrap();
    let (server, _requests, handle) = mock_server().await;
    let output = terminal_run(
        &fixture,
        &server,
        "run --no-autowire --yolo claude",
        "y\ny\n2\n\n",
    )
    .await;
    assert!(output.contains("ai-jail is installed"), "{output}");
    assert!(
        !output.contains("ai-jail: what the agent can reach"),
        "no checklist with a project .ai-jail:\n{output}"
    );
    let argv = fixture
        .jail_argv()
        .unwrap_or_else(|| panic!("ai-jail exec'd:\n{output}"));
    let (flags, _) = split_at_separator(&argv);
    assert_eq!(toggles(flags), Vec::<String>::new(), "{output}");
    assert!(!flags.iter().any(|flag| flag == "--docker"));
    handle.abort();
}

#[tokio::test]
async fn bare_nonterminal_command_help_version_and_typos_do_not_launch_a_picker() {
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    let (server, requests, handle) = mock_server().await;
    for (args, succeeds) in [
        (vec![], false),
        (vec!["--help"], true),
        (vec!["--version"], true),
        (vec!["resumee"], false),
    ] {
        let output = run(&fixture, &server, &args).await;
        assert_eq!(output.status.success(), succeeds, "{}", stderr(&output));
        assert!(
            !fixture.home.join("data").exists(),
            "parsing must not load config"
        );
    }
    assert!(requests.lock().unwrap().is_empty());
    assert!(fixture.jail_argv().is_none());
    assert!(!fixture.claude_ran.exists());
    handle.abort();
}

/// Run an installed ai-jail's actual PTY proxy without nesting a kernel sandbox.
/// The backend stub only executes the command after bwrap's separator.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "set AI_MEMORY_TEST_REAL_AI_JAIL to an ai-jail binary accepting a fixture BWRAP_BIN"]
async fn installed_ai_jail_proxy_displays_resume_yolo() {
    let jail = std::env::var_os("AI_MEMORY_TEST_REAL_AI_JAIL")
        .expect("AI_MEMORY_TEST_REAL_AI_JAIL points to the binary being tested");
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    std::os::unix::fs::symlink(BIN, fixture.bin.join("ai-memory")).unwrap();
    write_script(
        &fixture.bin.join("bwrap"),
        "if [ \"$1\" = --version ]; then echo 'bubblewrap 0.11.0'; exit 0; fi\nwhile [ $# -gt 0 ]; do\nif [ \"$1\" = -- ]; then shift; exec \"$@\"; fi\nshift\ndone\nexit 1\n",
    );
    let (server, requests, handle) = mock_server().await;
    let launch = inside_ai_jail_here();
    let output = terminal_run_program(
        &fixture,
        &server,
        Path::new(&jail),
        "--no-landlock --no-seccomp --no-rlimits --no-mise --no-save-config --no-gpu --no-docker --no-display ai-memory resume --yolo",
        if launch { "\x1b[C\r" } else { "\x1b" },
        Some("Resume workstream"),
    )
    .await;
    assert!(output.contains("Resume workstream"), "{output}");
    if launch {
        let native_args = fs::read_to_string(&fixture.claude_ran).unwrap();
        assert!(
            native_args.contains("--dangerously-skip-permissions"),
            "{output}"
        );
        assert!(
            !output.contains("Re-run this session inside it?"),
            "{output}"
        );
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.ends_with("/finish"))
        );
    } else {
        assert_eq!(requests.lock().unwrap().as_slice(), ["/workstream/recent"]);
        assert!(!fixture.claude_ran.exists());
    }
    handle.abort();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_yolo_displays_picker_without_scanning_git_status() {
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    std::os::unix::fs::symlink(BIN, fixture.bin.join("ai-memory")).unwrap();
    write_script(
        &fixture.bin.join("ai-jail"),
        &format!(
            "printf '%s\\n' \"$@\" > '{}'\nexec \"$@\"\n",
            fixture.jail_argv.display(),
        ),
    );
    let git_binary = host_tool("git");
    let status_called = fixture.home.join("git-status-called");
    fs::remove_file(fixture.bin.join("git")).unwrap();
    write_script(
        &fixture.bin.join("git"),
        &format!(
            "for arg in \"$@\"; do\nif [ \"$arg\" = status ]; then\nprintf scanned > '{}'\nfi\ndone\nexec '{}' \"$@\"\n",
            status_called.display(),
            git_binary.display(),
        ),
    );
    let (server, requests, handle) = mock_server().await;
    let output = terminal_run_program(
        &fixture,
        &server,
        &fixture.bin.join("ai-jail"),
        "ai-memory resume --yolo",
        "\x1b",
        Some("Resume workstream"),
    )
    .await;
    assert!(output.contains("Resume workstream"), "{output}");
    assert!(
        !status_called.exists(),
        "listing must not scan the working tree before drawing the picker"
    );
    assert_eq!(requests.lock().unwrap().as_slice(), ["/workstream/recent"]);
    assert_eq!(
        fixture.jail_argv().unwrap(),
        ["ai-memory", "resume", "--yolo"]
    );
    assert!(!fixture.claude_ran.exists());
    handle.abort();
}

/// Run `ai-memory <args>` under `script` so stdin and stderr are a real
/// terminal, feed `input`, and return everything the terminal showed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn terminal_run(fixture: &Fixture, server: &str, args: &str, input: &str) -> String {
    terminal_run_program(fixture, server, Path::new(BIN), args, input, None).await
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn terminal_run_program(
    fixture: &Fixture,
    server: &str,
    program: &Path,
    args: &str,
    input: &str,
    ready: Option<&str>,
) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let runner = fixture.home.join("runner.sh");
    fs::write(&runner, format!("exec \"$JAIL_E2E_BINARY\" {args}\n")).unwrap();
    let mut command = command(fixture, server, &[]);
    let script = PathBuf::from("/usr/bin/script");
    let command = command.as_std_mut();
    let mut terminal = std::process::Command::new(&script);
    terminal.env_clear();
    #[cfg(target_os = "macos")]
    terminal.args(["-q", "/dev/null", "/bin/sh"]).arg(&runner);
    #[cfg(target_os = "linux")]
    terminal
        .args(["-qec", "exec /bin/sh \"$JAIL_E2E_RUNNER\"", "/dev/null"])
        .env("JAIL_E2E_RUNNER", &runner);
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            terminal.env(key, value);
        }
    }
    for key in ["TERM", "USER", "LOGNAME"] {
        if let Some(value) = std::env::var_os(key) {
            terminal.env(key, value);
        }
    }
    terminal
        .env("HOME", &fixture.home)
        .env("JAIL_E2E_BINARY", program)
        .env("AI_MEMORY_NATIVE_BIN", BIN)
        .env("AI_MEMORY_RUN_AUTOWIRE", "false")
        .env("BWRAP_BIN", fixture.bin.join("bwrap"))
        .current_dir(&fixture.repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = tokio::process::Command::from(terminal)
        .kill_on_drop(true)
        .spawn()
        .expect("script provides a pseudo-terminal");
    let mut prefix = Vec::new();
    if let Some(ready) = ready {
        let stdout = child.stdout.as_mut().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut chunk = [0u8; 4096];
            while !String::from_utf8_lossy(&prefix).contains(ready) {
                let count = stdout.read(&mut chunk).await.unwrap();
                assert!(
                    count > 0,
                    "terminal exited before picker: {}",
                    String::from_utf8_lossy(&prefix)
                );
                prefix.extend_from_slice(&chunk[..count]);
            }
        })
        .await
        .expect("picker appeared");
    }
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).await.unwrap();
    stdin.flush().await.unwrap();
    // Keep stdin open briefly so `script` forwards the lines before EOF.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    drop(stdin);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("terminal run finished")
        .expect("wait for script");
    if ready.is_some() {
        assert!(
            output.status.success(),
            "{}{}{}",
            String::from_utf8_lossy(&prefix),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    format!(
        "{}{}{}",
        String::from_utf8_lossy(&prefix),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn help_with_maps() -> String {
    format!("{HELP_2_4_1}{HELP_2_5_0_EXTRA}{HELP_MAPS}")
}

/// `--jail-map` / `--jail-rw-map` reach ai-jail as `--map` / `--rw-map`
/// before the `--`, resolved against the checkout, and the summary names them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn jail_maps_named_folders_read_only_and_read_write() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(&help_with_maps(), true, None);
    let (server, _requests, handle) = mock_server().await;
    let tap = fixture.sibling("tap");
    let output = run(
        &fixture,
        &server,
        &[
            "run",
            "--no-autowire",
            "--jail=none",
            "--jail-map",
            "../ai-jail",
            "--jail-rw-map",
            tap.to_str().unwrap(),
            "claude",
        ],
    )
    .await;
    let text = stderr(&output);
    assert!(
        fixture.jail_argv().is_some(),
        "ai-jail was not exec'd (status {:?}):\n{text}",
        output.status
    );
    assert_eq!(
        fixture.mounts(),
        [
            ("--map".to_owned(), fixture.sibling("ai-jail")),
            ("--rw-map".to_owned(), tap),
        ],
        "{text}"
    );
    assert!(text.contains("folders: ai-jail (ro), tap (rw)"), "{text}");
    assert!(!fixture.claude_ran.exists());
    handle.abort();
}

/// Each refusal stops the launch before the sandbox or the harness starts,
/// and its message says what is wrong and what to do.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_jail_folders_stop_the_launch_with_a_way_forward() {
    if skip_inside_ai_jail() {
        return;
    }
    let (server, _requests, handle) = mock_server().await;
    let fixture = Fixture::new(&help_with_maps(), true, None);
    fs::create_dir_all(fixture.home.join(".ssh")).unwrap();
    for (args, expected) in [
        (
            vec![
                "run",
                "--no-autowire",
                "--jail",
                "--jail-map",
                "~/.ssh",
                "claude",
            ],
            "cannot mount ~/.ssh in the jail: it would expose",
        ),
        (
            vec![
                "run",
                "--no-autowire",
                "--jail",
                "--jail-map",
                "../nope",
                "claude",
            ],
            "cannot mount ../nope in the jail: the folder does not exist",
        ),
        (
            vec![
                "run",
                "--no-autowire",
                "--jail",
                "--jail-rw-map",
                ".",
                "claude",
            ],
            "the checkout is already mounted",
        ),
        (
            vec!["run", "--no-autowire", "--jail-map", "../ai-jail", "claude"],
            "need a jailed run: add --jail, or --yolo and accept the ai-jail offer",
        ),
    ] {
        let output = run(&fixture, &server, &args).await;
        let text = stderr(&output);
        assert!(!output.status.success(), "{args:?}: {text}");
        assert!(text.contains(expected), "{args:?}: {text}");
        assert!(
            fixture.jail_argv().is_none(),
            "{args:?}: the sandbox must not start"
        );
        assert!(
            !fixture.claude_ran.exists(),
            "{args:?}: the harness must not start"
        );
    }
    handle.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ai_jail_without_folder_mounts_says_to_update_it() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    let (server, _requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &[
            "run",
            "--no-autowire",
            "--jail",
            "--jail-map",
            "../ai-jail",
            "claude",
        ],
    )
    .await;
    let text = stderr(&output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("update ai-jail or drop --jail-map"), "{text}");
    assert!(fixture.jail_argv().is_none());
    handle.abort();
}

/// The checklist is remembered per checkout outside the repository: the next
/// launch shows the earlier answers, Enter keeps them, and the jailed run is
/// handed the command line that repeats them.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_checklist_remembers_answers_and_hands_in_the_next_run_command() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new(HELP_2_4_1, true, None);
    let (server, _requests, handle) = mock_server().await;
    let flipped = visible_capability_rows()[1];
    let first = terminal_run(
        &fixture,
        &server,
        "run --no-autowire --yolo claude",
        "y\ny\n2\n\n",
    )
    .await;
    let first_argv = fixture
        .jail_argv()
        .unwrap_or_else(|| panic!("ai-jail exec'd:\n{first}"));
    assert!(first_argv.contains(&format!("--{flipped}")), "{first}");
    assert!(fixture.home.join("data/jail-choices.json").is_file());
    assert!(
        !fixture.repo.join(".ai-jail").exists(),
        "nothing is written into the repository"
    );
    assert_eq!(
        fs::read_to_string(&fixture.next_run).unwrap(),
        format!("ai-memory run --yolo --no-autowire --jail={flipped} claude")
    );
    assert!(
        first_argv
            .windows(2)
            .any(|pair| pair == ["--env", "AI_MEMORY_JAIL_NEXT_RUN"]),
        "{first_argv:?}"
    );

    fs::remove_file(&fixture.jail_argv).unwrap();
    let second = terminal_run(
        &fixture,
        &server,
        "run --no-autowire --yolo claude",
        "y\ny\n\n",
    )
    .await;
    assert!(second.contains("your choices from"), "{second}");
    let second_argv = fixture
        .jail_argv()
        .unwrap_or_else(|| panic!("ai-jail exec'd:\n{second}"));
    let (first_flags, _) = split_at_separator(&first_argv);
    let (second_flags, _) = split_at_separator(&second_argv);
    assert_eq!(
        toggles(first_flags),
        toggles(second_flags),
        "Enter keeps the remembered answers"
    );

    fs::remove_file(&fixture.jail_argv).unwrap();
    let reset = terminal_run(
        &fixture,
        &server,
        "run --no-autowire --yolo claude",
        "y\ny\nreset\n\n",
    )
    .await;
    let reset_argv = fixture
        .jail_argv()
        .unwrap_or_else(|| panic!("ai-jail exec'd:\n{reset}"));
    assert!(reset_argv.contains(&format!("--no-{flipped}")), "{reset}");
    handle.abort();
}

/// `--help` of an ai-jail that speaks the wrapper contract (3.0.1).
fn help_with_contract() -> String {
    format!(
        "{HELP_2_4_1}{HELP_2_5_0_EXTRA}{HELP_MAPS}\
    --harness <NAME>               Declare the effective harness explicitly
    --request-file <PATH>          Load a versioned wrapper request manifest
    --forward-port <PORT>          Expose the host's 127.0.0.1:PORT on the sandbox's own
"
    )
}

/// `capabilities --json` of ai-jail 3.0.1.
const CAPABILITIES: &str = r#"{"version":"3.0.1","schema_version":1,
"flags":["--harness","--request-file","--request","--json"],
"request_kinds":["private_host","host","env","secret","agent_state","map"],
"features":{"harness_flag":true,"request_file":true,"status_json":true,
"secret_private_http":true,"configure_requests":true,"inside_marker":false},
"exit_codes":{"cancelled":130,"required_grant":3,"blocked":4,"backend_unavailable":5}}"#;

/// `status --json` for a project whose review is done and approves everything.
const STATUS_REVIEWED: &str = r#"{"version":"3.0.1","schema_version":1,
"project":{"canonical_path":"/p","profile_present":true},
"harnesses":[{"name":"claude","onboarding_complete":true}],
"grants":{"restricted":[],"allowed":[],"open":[]},
"requests":{"approved":[],"pending":[]},"blockers":[]}"#;

/// The same project after ai-memory started asking for one more variable.
const STATUS_NEW_REQUEST: &str = r#"{"version":"3.0.1","schema_version":1,
"project":{"canonical_path":"/p","profile_present":true},
"harnesses":[{"name":"claude","onboarding_complete":true}],
"grants":{"restricted":[],"allowed":[],"open":[]},
"requests":{"approved":[],"pending":[{"requester":"ai-memory","kind":"env","value":"AI_MEMORY_AUTH_TOKEN"}]},
"blockers":[{"code":"required_grant_not_approved","message":"One or more wrapper requests are pending approval"}]}"#;

impl Fixture {
    /// Turn the fake into ai-jail 3.0.1: it answers `capabilities --json` and,
    /// for `status --json`, prints `status` and records that argv too.
    fn with_contract(self, status: &str) -> Self {
        let root = self.repo.parent().unwrap().to_path_buf();
        let help = root.join("ai-jail-help.txt");
        fs::write(&help, help_with_contract()).unwrap();
        let capabilities = root.join("ai-jail-capabilities.json");
        fs::write(&capabilities, CAPABILITIES).unwrap();
        let status_file = root.join("ai-jail-status.json");
        fs::write(&status_file, status).unwrap();
        write_script(
            &self.bin.join("ai-jail"),
            &format!(
                "if [ \"$1\" = --help ]; then cat '{help}'; exit 0; fi\n\
                 if [ \"$1\" = --dry-run ]; then exit 0; fi\n\
                 if [ \"$1\" = capabilities ]; then cat '{caps}'; exit 0; fi\n\
                 if [ \"$1\" = configure ]; then printf '%s\\n' \"$@\" > '{configure}'; exit 0; fi\n\
                 for arg in \"$@\"; do\n\
                 [ \"$arg\" = -- ] && break\n\
                 if [ \"$arg\" = status ]; then printf '%s\\n' \"$@\" > '{status_argv}'; cat '{status}'; exit 0; fi\n\
                 done\n\
                 printf '%s\\n' \"$@\" > '{argv}'\n\
                 printf '%s' \"$AI_MEMORY_JAIL_NEXT_RUN\" > '{next}'\n",
                help = help.display(),
                caps = capabilities.display(),
                configure = root.join("ai-jail-configure.txt").display(),
                status_argv = root.join("ai-jail-status-argv.txt").display(),
                status = status_file.display(),
                argv = self.jail_argv.display(),
                next = self.next_run.display(),
            ),
        );
        self
    }

    fn recorded(&self, name: &str) -> Option<Vec<String>> {
        fs::read_to_string(self.repo.parent().unwrap().join(name))
            .ok()
            .map(|text| text.lines().map(str::to_owned).collect())
    }
}

/// With ai-jail 3.0.1 the launch declares the harness, hands over a request
/// manifest from ai-memory's data directory, forwards only the loopback
/// server's port instead of opening the network, and asks for the harness
/// state and the variables as requests rather than granting them by flag.
/// The read-only status call previews exactly that launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ai_jail_3_gets_the_harness_and_a_request_file_instead_of_grants() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new("", true, None).with_contract(STATUS_REVIEWED);
    let (server, requests, handle) = mock_server().await;
    let port = server.rsplit(':').next().unwrap().to_owned();
    let output = run(
        &fixture,
        &server,
        &["run", "--no-autowire", "claude", "--jail=github"],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let argv = fixture
        .jail_argv()
        .unwrap_or_else(|| panic!("ai-jail was never exec'd:\n{}", stderr(&output)));
    let (flags, wrapped) = split_at_separator(&argv);
    let manifest = fixture.home.join("data").join("jail-requests");
    assert_eq!(flags[..2], ["--harness", "claude"], "{flags:?}");
    assert_eq!(flags[2], "--request-file");
    assert!(
        Path::new(&flags[3]).starts_with(&manifest),
        "the manifest lives in the data directory, outside the checkout: {flags:?}"
    );
    assert_eq!(
        flags[4..],
        [
            "--forward-port",
            port.as_str(),
            "--no-save-config",
            "--github"
        ],
        "a loopback server gets its port forwarded, never the whole network"
    );
    for granted in ["--network", "--agent-state", "--env"] {
        assert!(
            !flags.iter().any(|flag| flag == granted),
            "{granted}: {flags:?}"
        );
    }
    assert_eq!(
        wrapped,
        [
            exe().as_str(),
            "run",
            "--no-autowire",
            "claude",
            "--jail=github"
        ]
    );
    let body: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&flags[3]).unwrap()).unwrap();
    assert_eq!(body["version"], 1);
    assert_eq!(body["requester"], "ai-memory");
    let asked: Vec<(String, String)> = body["requests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["kind"].as_str().unwrap().to_owned(),
                item["value"].to_string(),
            )
        })
        .collect();
    assert_eq!(asked[0], ("agent_state".to_owned(), "true".to_owned()));
    assert!(
        asked.contains(&("env".to_owned(), "\"AI_MEMORY_SERVER_URL\"".to_owned())),
        "{asked:?}"
    );
    let status = fixture
        .recorded("ai-jail-status-argv.txt")
        .expect("status ran");
    let (status_flags, status_wrapped) = split_at_separator(&status);
    assert_eq!(status_flags[..status_flags.len() - 2], *flags);
    assert_eq!(status_flags[status_flags.len() - 2..], ["status", "--json"]);
    assert_eq!(status_wrapped, wrapped);
    assert!(
        fixture.recorded("ai-jail-configure.txt").is_none(),
        "nothing pending, so no review"
    );
    assert!(
        stderr(&output).contains("ai-jail 3.0.1 reviews the rest of the access for claude"),
        "{}",
        stderr(&output)
    );
    assert!(requests.lock().unwrap().is_empty());
    handle.abort();
}

/// A reviewed project that gains a request cannot launch until ai-jail's
/// review approves it, and that review needs a terminal: without one the
/// launch stops before exec and prints the exact review command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_request_without_a_terminal_stops_with_the_review_command() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new("", true, None).with_contract(STATUS_NEW_REQUEST);
    let (server, _requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &["run", "--no-autowire", "claude", "--jail"],
    )
    .await;
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(text.contains("needs a terminal"), "{text}");
    assert!(
        text.contains("configure --harness claude --request-file"),
        "{text}"
    );
    assert!(fixture.jail_argv().is_none(), "nothing was launched");
    assert!(fixture.recorded("ai-jail-configure.txt").is_none());
    handle.abort();
}

/// A harness ai-jail has no preset for cannot be declared (ai-jail refuses
/// an unknown `--harness`), so it keeps the flag-based launch, and no status
/// call is made for a contract it is not using.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_harness_without_an_ai_jail_preset_keeps_the_flag_launch() {
    if skip_inside_ai_jail() {
        return;
    }
    let fixture = Fixture::new("", true, None).with_contract(STATUS_REVIEWED);
    write_script(&fixture.bin.join("copilot"), "exit 0\n");
    let (server, _requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &["run", "--no-autowire", "copilot", "--jail"],
    )
    .await;
    let argv = fixture
        .jail_argv()
        .unwrap_or_else(|| panic!("ai-jail was never exec'd:\n{}", stderr(&output)));
    let (flags, _) = split_at_separator(&argv);
    assert!(
        !flags
            .iter()
            .any(|flag| flag == "--harness" || flag == "--request-file"),
        "{flags:?}"
    );
    assert_eq!(
        flags[..3],
        ["--network", "--agent-state", "--no-save-config"]
    );
    assert!(fixture.recorded("ai-jail-status-argv.txt").is_none());
    handle.abort();
}

/// A saved approval that no longer matches the host (here an approved
/// variable that is not set any more) stops the launch before exec, quoting
/// ai-jail's reason and the review command, instead of letting ai-jail fail
/// inside the re-exec with exit 4.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drifted_ai_jail_access_stops_before_the_launch_with_the_reason() {
    if skip_inside_ai_jail() {
        return;
    }
    let status = STATUS_REVIEWED.replace(
        r#""blockers":[]"#,
        r#""blockers":[{"code":"missing_approved_env","message":"approved variable OPENAI_API_KEY is not set"}]"#,
    );
    assert_ne!(status, STATUS_REVIEWED);
    let fixture = Fixture::new("", true, None).with_contract(&status);
    let (server, _requests, handle) = mock_server().await;
    let output = run(
        &fixture,
        &server,
        &["run", "--no-autowire", "claude", "--jail"],
    )
    .await;
    assert!(!output.status.success());
    let text = stderr(&output);
    assert!(
        text.contains(
            "no longer matches this machine: approved variable OPENAI_API_KEY is not set"
        ),
        "{text}"
    );
    assert!(
        text.contains("configure --harness claude --request-file"),
        "{text}"
    );
    assert!(fixture.jail_argv().is_none(), "nothing was launched");
    handle.abort();
}
