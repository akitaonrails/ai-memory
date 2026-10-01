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

struct Fixture {
    _temp: tempfile::TempDir,
    repo: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    jail_argv: PathBuf,
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
        let claude_ran = root.join("claude-ran.txt");
        // `--help` feeds toggle support; `--dry-run` is the usable-ai-jail
        // preflight (`usable_ai_jail_here`). Neither is a real re-exec, so
        // neither may touch `jail_argv` — tests assert its absence when the
        // wrapper stays unjailed or rejects before exec.
        write_script(
            &bin.join("ai-jail"),
            &format!(
                "if [ \"$1\" = --help ]; then cat '{}'; exit 0; fi\n\
                 if [ \"$1\" = --dry-run ]; then exit 0; fi\n\
                 printf '%s\\n' \"$@\" > '{}'\n",
                help_file.display(),
                jail_argv.display()
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
            claude_ran,
        }
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
            if uri.path() == "/workstream/runs" {
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
    assert!(output.contains("Enable in the jail"), "{output}");
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
        !output.contains("Enable in the jail"),
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

/// Run `ai-memory <args>` under `script` so stdin and stderr are a real
/// terminal, feed `input`, and return everything the terminal showed.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn terminal_run(fixture: &Fixture, server: &str, args: &str, input: &str) -> String {
    use tokio::io::AsyncWriteExt as _;
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
        .env("JAIL_E2E_BINARY", BIN)
        .current_dir(&fixture.repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = tokio::process::Command::from(terminal)
        .spawn()
        .expect("script provides a pseudo-terminal");
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
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
