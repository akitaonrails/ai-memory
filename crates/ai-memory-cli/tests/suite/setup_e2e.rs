//! `ai-memory setup` through the shipped binary, in a temp home.
//!
//! The service manager is a test double: a `systemctl` script first on `PATH`
//! that records every call and, on `restart`, runs the rendered unit's
//! `ExecStart` in the background. No real user session, unit directory or
//! harness config is touched.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use crate::e2e_support::hermetic;

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");

/// A temp root holding the operator's home and a directory of fake tools.
struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        Self {
            _tmp: tmp,
            root,
            home,
        }
    }

    /// The default data dir `setup` resolves in this home: macOS uses
    /// `~/Library/Application Support`, other Unix the XDG data dir.
    fn data_dir(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join("Library/Application Support/ai-memory")
        } else {
            self.home.join(".local/share/ai-memory")
        }
    }

    /// Every file under the home with its contents, the database and logs
    /// aside (the running server owns those).
    fn snapshot(&self) -> Vec<(PathBuf, Vec<u8>)> {
        let skip = [self.data_dir().join("db"), self.data_dir().join("logs")];
        let mut files = Vec::new();
        let mut pending = vec![self.home.clone()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if skip.iter().any(|s| path.starts_with(s)) {
                    continue;
                }
                if path.is_dir() {
                    pending.push(path);
                } else {
                    files.push((path.clone(), std::fs::read(&path).unwrap_or_default()));
                }
            }
        }
        files.sort();
        files
    }

    fn cmd(&self, port: Option<u16>) -> Command {
        let mut cmd = hermetic(BIN);
        cmd.env("HOME", &self.home)
            .env("AI_MEMORY_HOME", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
            .env("AI_MEMORY_BACKFILL_ON_START", "false")
            .env("AI_MEMORY_AUTO_IMPROVE__SCHEDULER__ENABLED", "false")
            .env("RUST_LOG", "off")
            .env_remove("CLAUDE_CODE_SESSION_ID")
            .env_remove("MUSE_SESSION_ID")
            .current_dir(&self.home)
            .stdin(Stdio::null());
        if let Some(port) = port {
            // Never the default port: a real server may be listening there.
            cmd.env("AI_MEMORY_BIND", format!("127.0.0.1:{port}"))
                .env("AI_MEMORY_SERVER_URL", format!("http://127.0.0.1:{port}"));
        }
        cmd
    }
}

fn text(out: &Output) -> String {
    format!(
        "status: {}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Without `--yes` and with no terminal on stdin, `setup` shows the plan,
/// changes nothing and exits non-zero naming `--yes`.
#[test]
fn setup_without_yes_and_without_a_terminal_changes_nothing() {
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.home.join(".claude/projects")).unwrap();
    let before = sandbox.snapshot();
    let port = crate::e2e_support::free_port();
    let out = sandbox.cmd(Some(port)).arg("setup").output().unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("ai-memory setup will:"), "{}", text(&out));
    assert!(
        stdout.contains("1. claude-code: MCP + hooks + skills (found its session store at"),
        "{}",
        text(&out)
    );
    assert!(
        stdout.contains("not in this plan (add with --agent <name>):") && stdout.contains("cursor"),
        "{}",
        text(&out)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--yes"),
        "{}",
        text(&out)
    );
    // Any command opens its log under the data dir; nothing else may appear.
    assert!(!sandbox.data_dir().join("config.toml").exists());
    assert!(!sandbox.data_dir().join("auth-tokens").exists());
    assert_eq!(sandbox.snapshot(), before, "setup wrote without --yes");
}

/// `--plan --json` prints a parseable plan naming the detected agent and its
/// evidence, exits zero, and writes nothing, with no terminal and no `--yes`.
#[test]
fn setup_plan_json_prints_the_plan_and_changes_nothing() {
    let sandbox = Sandbox::new();
    let store = sandbox.home.join(".claude/projects");
    std::fs::create_dir_all(&store).unwrap();
    let before = sandbox.snapshot();
    let port = crate::e2e_support::free_port();
    let out = sandbox
        .cmd(Some(port))
        .args(["setup", "--plan", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON plan");
    assert_eq!(plan["format"], 1, "{plan}");
    assert_eq!(plan["fresh"], true, "{plan}");
    assert_eq!(plan["agents"][0]["agent"], "claude-code", "{plan}");
    assert_eq!(
        plan["agents"][0]["found"]["kind"], "session_store",
        "{plan}"
    );
    assert_eq!(
        plan["agents"][0]["found"]["path"],
        store.display().to_string(),
        "{plan}"
    );
    assert!(!sandbox.data_dir().join("config.toml").exists());
    assert_eq!(sandbox.snapshot(), before, "setup --plan wrote");

    // `--plan` cannot be combined with `--yes`, and `--json` needs `--plan`.
    for args in [&["setup", "--plan", "--yes"][..], &["setup", "--json"][..]] {
        let refused = sandbox.cmd(Some(port)).args(args).output().unwrap();
        assert!(!refused.status.success(), "{args:?}: {}", text(&refused));
    }
    assert_eq!(sandbox.snapshot(), before, "a refused setup wrote");
}

/// An SPA can answer every unknown route with HTML and HTTP 200. It is not
/// an existing ai-memory install, and its MCP/admin errors are not useful
/// setup diagnostics.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_identifies_a_foreign_listener_in_the_plan_and_self_check() {
    let sandbox = Sandbox::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = paths.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        seen.lock().unwrap().push(uri.path().to_owned());
        async { axum::response::Html("<!doctype html><title>Another service</title>") }
    });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let planned = sandbox
        .cmd(Some(port))
        .args(["setup", "--plan", "--json"])
        .output()
        .unwrap();
    assert!(planned.status.success(), "{}", text(&planned));
    let plan: serde_json::Value = serde_json::from_slice(&planned.stdout).unwrap();
    assert_eq!(plan["service"]["action"], "skip", "{plan}");
    let reason = plan["service"]["reason"].as_str().unwrap();
    assert!(
        reason.contains("did not return ai-memory's /healthz"),
        "{reason}"
    );
    assert!(reason.contains(&format!("127.0.0.1:{port}")), "{reason}");
    assert!(
        reason.contains("config.toml") && reason.contains("bind"),
        "{reason}"
    );

    let applied = sandbox
        .cmd(Some(port))
        .args(["setup", "--yes"])
        .output()
        .unwrap();
    server.abort();
    assert!(!applied.status.success(), "{}", text(&applied));
    let stdout = String::from_utf8_lossy(&applied.stdout);
    assert!(!stdout.contains("healthz: ok"), "{}", text(&applied));
    assert!(
        stdout.contains("did not return ai-memory's /healthz"),
        "{}",
        text(&applied)
    );
    assert!(
        paths.lock().unwrap().iter().all(|path| path == "/healthz"),
        "setup sent unrelated probes to the foreign listener: {:?}",
        paths.lock().unwrap()
    );
    assert!(
        !sandbox
            .home
            .join("Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist")
            .exists()
    );
    assert!(
        !sandbox
            .home
            .join(".config/systemd/user/ai-memory.service")
            .exists()
    );
}

/// The generic `opencode` agent, named or found by the session store both
/// majors share, is wired in the major `opencode --version` reports: OpenCode
/// 2 gets the `{ id, setup }` plugin and the nested `mcp.servers` entry, not
/// the V1 function plugin and direct `mcp` entry. No server answers here, so
/// each run still exits non-zero on its self-check.
#[test]
fn setup_wires_the_opencode_major_the_version_probe_reports() {
    use std::os::unix::fs::PermissionsExt as _;

    for (version, named, plugin, stale, v2) in [
        (
            "2.0.31",
            true,
            "ai-memory-opencode2.ts",
            "ai-memory.ts",
            true,
        ),
        (
            "2.0.31",
            false,
            "ai-memory-opencode2.ts",
            "ai-memory.ts",
            true,
        ),
        (
            "1.18.34",
            true,
            "ai-memory.ts",
            "ai-memory-opencode2.ts",
            false,
        ),
    ] {
        let sandbox = Sandbox::new();
        let bin = sandbox.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let opencode = bin.join("opencode");
        std::fs::write(&opencode, format!("#!/bin/sh\necho '{version}'\n")).unwrap();
        std::fs::set_permissions(&opencode, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(sandbox.home.join(".local/share/opencode")).unwrap();
        let mut cmd = sandbox.cmd(Some(crate::e2e_support::free_port()));
        cmd.env("PATH", &bin)
            .args(["setup", "--yes", "--no-service"]);
        if named {
            cmd.args(["--agent", "opencode"]);
        }
        let out = cmd.output().unwrap();
        let case = format!("{version} named={named}\n{}", text(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("== open-code"),
            "{case}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("problem: open-code"),
            "{case}"
        );

        let config = sandbox.home.join(".config/opencode");
        assert!(config.join("plugins").join(plugin).is_file(), "{case}");
        assert!(!config.join("plugins").join(stale).exists(), "{case}");
        let mcp: serde_json::Value =
            serde_json::from_slice(&std::fs::read(config.join("opencode.json")).unwrap()).unwrap();
        assert_eq!(
            mcp["mcp"]["servers"]["ai-memory"].is_object(),
            v2,
            "{case}\n{mcp}"
        );
        assert_eq!(mcp["mcp"]["ai-memory"].is_object(), !v2, "{case}\n{mcp}");
    }
}

/// With no `opencode` to probe, setup does not guess a major: it writes no
/// OpenCode plugin or MCP entry and names the problem.
#[test]
fn setup_without_an_opencode_to_probe_writes_no_opencode_wiring() {
    let sandbox = Sandbox::new();
    let empty = sandbox.root.join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    let out = sandbox
        .cmd(Some(crate::e2e_support::free_port()))
        .env("PATH", &empty)
        .args(["setup", "--yes", "--no-service", "--agent", "opencode"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{}", text(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("problem: open-code: skipped install-mcp and install-hooks")
            && stderr.contains("was not found"),
        "{}",
        text(&out)
    );
    assert!(
        !sandbox.home.join(".config/opencode").exists(),
        "{}",
        text(&out)
    );
}

/// `setup --web` through the binary: the printed address serves the page and
/// plan, a foreign Host is refused, Apply runs `setup --yes --agent
/// claude-code` (which writes Claude Code's hooks), and Finish stops the
/// process. No server runs here, so that setup reports its failed self-check
/// and the wizard exits non-zero after Finish.
#[test]
fn setup_web_serves_the_plan_and_applies_the_picked_agent() {
    use std::io::{BufRead as _, Read as _, Write as _};
    use std::net::TcpStream;

    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.home.join(".claude/projects")).unwrap();
    let port = crate::e2e_support::free_port();
    let mut child = sandbox
        .cmd(Some(port))
        .args(["setup", "--web", "--no-open", "--no-service"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    struct Kill(std::process::Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let stdout = child.stdout.take().unwrap();
    let mut child = Kill(child);
    let mut url = String::new();
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.unwrap();
        if let Some(found) = line.trim().strip_prefix("http://127.0.0.1:") {
            url = format!("http://127.0.0.1:{found}");
            break;
        }
    }
    assert!(url.ends_with('/'), "no address printed: {url:?}");
    let authority = url
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap()
        .to_owned();
    let path = url
        .trim_start_matches(&format!("http://{authority}"))
        .to_owned();

    let request = |method: &str, rest: &str, host: &str, extra: &str, body: &str| -> String {
        let mut stream = TcpStream::connect(&authority).unwrap();
        write!(
            stream,
            "{method} {path}{rest} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n{extra}\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
    };
    let page = request("GET", "", &authority, "", "");
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");
    assert!(page.contains("ai-memory setup"), "{page}");
    let rebound = request("GET", "", "attacker.example", "", "");
    assert!(rebound.starts_with("HTTP/1.1 403"), "{rebound}");

    let plan = request("GET", "plan", &authority, "", "");
    let json: serde_json::Value =
        serde_json::from_str(plan.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(json["plan"]["agents"][0]["agent"], "claude-code", "{json}");
    let csrf = json["csrf"].as_str().unwrap().to_owned();
    let headers = format!("Origin: http://{authority}\r\nx-ai-memory-setup: {csrf}\r\n");

    let applied = request(
        "POST",
        "apply",
        &authority,
        &headers,
        r#"{"agents":["claude-code"]}"#,
    );
    assert!(applied.starts_with("HTTP/1.1 202"), "{applied}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let progress = loop {
        let answer = request("GET", "progress?from=0", &authority, "", "");
        let json: serde_json::Value =
            serde_json::from_str(answer.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        if json["done"] == true {
            break json;
        }
        assert!(std::time::Instant::now() < deadline, "setup never finished");
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let lines = progress["lines"].to_string();
    assert!(lines.contains("== claude-code"), "{lines}");
    assert!(
        sandbox.home.join(".claude/settings.json").is_file(),
        "{lines}"
    );

    let finished = request("POST", "finish", &authority, &headers, "{}");
    assert!(finished.starts_with("HTTP/1.1 204"), "{finished}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the wizard did not stop"
        );
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert_eq!(progress["ok"] == true, status.success(), "{lines}");
}

/// `setup --web --stop-on-stdin-eof` stops on its own once the process that
/// started it closes its stdin (the desktop installer going away), with no
/// Finish and no signal; without the flag, closing stdin does not stop it.
#[test]
fn setup_web_stops_when_its_stdin_closes_only_when_asked() {
    use std::io::BufRead as _;
    let sandbox = Sandbox::new();
    let port = crate::e2e_support::free_port();
    let start = |flag: Option<&str>| {
        let mut cmd = sandbox.cmd(Some(port));
        cmd.args(["setup", "--web", "--no-open", "--no-service"]);
        if let Some(flag) = flag {
            cmd.arg(flag);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        for line in std::io::BufReader::new(stdout).lines() {
            if line.unwrap().trim().starts_with("http://127.0.0.1:") {
                break;
            }
        }
        child
    };
    let wait = |child: &mut std::process::Child, within: std::time::Duration| {
        let deadline = std::time::Instant::now() + within;
        while std::time::Instant::now() < deadline {
            if child.try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    };

    let mut watched = start(Some("--stop-on-stdin-eof"));
    drop(watched.stdin.take());
    assert!(
        wait(&mut watched, std::time::Duration::from_secs(20)),
        "the page outlived its parent"
    );

    let mut unwatched = start(None);
    drop(unwatched.stdin.take());
    let stopped = wait(&mut unwatched, std::time::Duration::from_secs(2));
    let _ = unwatched.kill();
    let _ = unwatched.wait();
    assert!(!stopped, "without the flag, closing stdin must not stop it");
}

/// Spawns a server through the fake service manager: the slow tier.
#[cfg(target_os = "linux")]
mod slow {
    use super::*;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// `systemctl --user` stand-in. `restart` runs the unit's `ExecStart`
    /// (the rendered paths carry no `%` or `$`, so shell word splitting
    /// matches systemd's here).
    const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
state="${FAKE_SYSTEMD_STATE:?}"
mkdir -p "$state"
printf '%s\n' "$*" >> "$state/calls"
[ "$1" = "--user" ] || exit 2
shift
unit="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ai-memory.service"
alive() { [ -f "$state/pid" ] && kill -0 "$(cat "$state/pid")" 2>/dev/null; }
case "$1" in
  daemon-reload) ;;
  enable) touch "$state/enabled" ;;
  is-enabled) [ -f "$state/enabled" ] ;;
  is-active) alive ;;
  disable)
    rm -f "$state/enabled"
    if [ "$2" = "--now" ] && alive; then kill "$(cat "$state/pid")"; rm -f "$state/pid"; fi ;;
  restart)
    if alive; then kill "$(cat "$state/pid")"; sleep 0.2; fi
    line="$(sed -n 's/^ExecStart=//p' "$unit")"
    eval "set -- $line"
    "$@" >"$state/server.log" 2>&1 &
    echo $! > "$state/pid"
    ;;
  *) exit 3 ;;
esac
"#;

    struct Service {
        state: PathBuf,
        bin: PathBuf,
    }

    impl Service {
        fn new(root: &Path) -> Self {
            use std::os::unix::fs::PermissionsExt as _;
            let bin = root.join("fakebin");
            std::fs::create_dir_all(&bin).unwrap();
            let script = bin.join("systemctl");
            std::fs::write(&script, FAKE_SYSTEMCTL).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                state: root.join("systemd"),
                bin,
            }
        }

        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.state.join("calls"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn apply(&self, cmd: &mut Command) {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut joined = std::ffi::OsString::from(&self.bin);
            joined.push(":");
            joined.push(path);
            cmd.env("PATH", joined)
                .env("FAKE_SYSTEMD_STATE", &self.state);
        }
    }

    impl Drop for Service {
        fn drop(&mut self) {
            if let Ok(pid) = std::fs::read_to_string(self.state.join("pid")) {
                let _ = Command::new("kill").arg(pid.trim()).status();
            }
        }
    }

    fn setup(sandbox: &Sandbox, service: &Service, port: u16, args: &[&str]) -> Output {
        let mut cmd = sandbox.cmd(Some(port));
        service.apply(&mut cmd);
        cmd.arg("setup").args(args).output().unwrap()
    }

    async fn captured_sessions(base: &str, token: &str, workspace: &str, project: &str) -> u64 {
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let resp = client
                .get(format!("{base}/admin/sessions/by-agent"))
                .bearer_auth(token)
                .query(&[("workspace", workspace), ("project", project)])
                .send()
                .await
                .unwrap();
            let count = if resp.status().is_success() {
                let body: Value = resp.json().await.unwrap();
                body["by_agent"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x["sessions"].as_u64()).sum())
                    .unwrap_or(0)
            } else {
                0
            };
            if count > 0 || Instant::now() >= deadline {
                return count;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Fresh home with Claude Code present: one `setup --yes` initialises the
    /// install with a token, writes and starts the user unit on this install,
    /// wires MCP, hooks and skills without the token in any harness file, and
    /// passes its own self-check. A captured hook event reaches the server. A
    /// second run changes no file and makes no state-changing service call.
    #[tokio::test]
    async fn setup_installs_wires_is_idempotent_and_uninstall_removes_its_unit() {
        let sandbox = Sandbox::new();
        let service = Service::new(&sandbox.root);
        std::fs::create_dir_all(sandbox.home.join(".claude/projects")).unwrap();
        let repo = sandbox.home.join("work/repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(".ai-memory.toml"),
            "workspace = \"setup-ws\"\nproject = \"setup-project\"\n",
        )
        .unwrap();
        let port = crate::e2e_support::free_port();
        let base = format!("http://127.0.0.1:{port}");

        let first = setup(&sandbox, &service, port, &["--yes"]);
        assert!(first.status.success(), "{}", text(&first));
        let stdout = String::from_utf8_lossy(&first.stdout);
        assert!(stdout.contains("healthz: ok"), "{}", text(&first));
        assert!(stdout.contains("memory_status: ok"), "{}", text(&first));
        assert!(
            stdout.contains("What do you remember about this project?"),
            "{}",
            text(&first)
        );

        let data_dir = sandbox.data_dir();
        let token = std::fs::read_to_string(data_dir.join("auth-tokens/default"))
            .unwrap()
            .trim()
            .to_owned();
        assert!(!token.is_empty());
        assert!(!stdout.contains(&token), "setup printed the token");
        let unit =
            std::fs::read_to_string(sandbox.home.join(".config/systemd/user/ai-memory.service"))
                .unwrap();
        let exec = unit
            .lines()
            .find(|line| line.starts_with("ExecStart="))
            .unwrap();
        assert!(
            exec.contains(&format!("--data-dir \"{}\"", data_dir.display())),
            "{exec}"
        );
        assert!(
            exec.contains(&format!(
                "--config \"{}\"",
                data_dir.join("config.toml").display()
            )),
            "{exec}"
        );
        assert!(!unit.contains(&token), "the unit holds the token");
        assert_eq!(
            service.calls(),
            vec![
                "--user is-active --quiet ai-memory.service",
                "--user daemon-reload",
                "--user enable ai-memory.service",
                "--user restart ai-memory.service",
            ]
        );

        // The token lives only in the token file and the hook credential files.
        let allowed = [
            data_dir.join("auth-tokens/default"),
            data_dir.join("auth-token"),
            data_dir.join("auth-header"),
            data_dir.join("hook-spool"),
            data_dir.join("db"),
        ];
        let leaks: Vec<_> = sandbox
            .snapshot()
            .into_iter()
            .filter(|(path, bytes)| {
                !allowed.iter().any(|a| path.starts_with(a))
                    && bytes.windows(token.len()).any(|w| w == token.as_bytes())
            })
            .map(|(path, _)| path)
            .collect();
        assert!(leaks.is_empty(), "token found in {leaks:?}");

        // Wired: MCP through the bridge, hooks, skills.
        let claude: Value = serde_json::from_str(
            &std::fs::read_to_string(sandbox.home.join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(claude["mcpServers"]["ai-memory"]["args"][0], "mcp-bridge");
        let settings = std::fs::read_to_string(sandbox.home.join(".claude/settings.json")).unwrap();
        assert!(settings.contains("user-prompt-submit"), "{settings}");
        assert!(
            sandbox
                .home
                .join(".claude/skills/ai-memory-retrieval/SKILL.md")
                .is_file()
        );

        // A hook event, the way the installed hook sends it, is captured.
        let mut hook = sandbox.cmd(Some(port));
        let mut child = hook
            .args([
                "hook",
                "--event",
                "user-prompt-submit",
                "--agent",
                "claude-code",
                "--server-url",
                &base,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        {
            use std::io::Write as _;
            let payload = json!({
                "session_id": "setup-e2e-session",
                "cwd": repo,
                "prompt": "setup e2e prompt",
            });
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.to_string().as_bytes())
                .unwrap();
        }
        assert!(child.wait().unwrap().success());
        let drain = sandbox.cmd(Some(port)).arg("hook-drain").output().unwrap();
        assert!(drain.status.success(), "{}", text(&drain));
        assert_eq!(
            captured_sessions(&base, &token, "setup-ws", "setup-project").await,
            1
        );

        // Second run: same bytes everywhere, no state-changing call.
        let before = sandbox.snapshot();
        let calls_before = service.calls().len();
        let second = setup(&sandbox, &service, port, &["--yes"]);
        assert!(second.status.success(), "{}", text(&second));
        assert!(
            String::from_utf8_lossy(&second.stdout).contains("already running"),
            "{}",
            text(&second)
        );
        let after = sandbox.snapshot();
        let changed: Vec<_> = before
            .iter()
            .zip(after.iter())
            .filter(|(a, b)| a != b)
            .map(|(a, _)| a.0.clone())
            .collect();
        assert_eq!(
            before.len(),
            after.len(),
            "the second run added or removed files"
        );
        assert!(changed.is_empty(), "the second run changed {changed:?}");
        assert_eq!(
            service.calls()[calls_before..].to_vec(),
            vec![
                "--user is-active --quiet ai-memory.service",
                "--user is-enabled --quiet ai-memory.service",
            ]
        );

        // uninstall stops and removes the unit setup wrote, and only once.
        let unit_path = sandbox.home.join(".config/systemd/user/ai-memory.service");
        let calls_before = service.calls().len();
        let removed = uninstall(&sandbox, &service, port);
        assert!(removed.status.success(), "{}", text(&removed));
        assert!(
            String::from_utf8_lossy(&removed.stdout)
                .contains("stopped and removed the user service"),
            "{}",
            text(&removed)
        );
        assert!(!unit_path.exists());
        assert_eq!(
            service.calls()[calls_before..].to_vec(),
            vec![
                "--user disable --now ai-memory.service",
                "--user daemon-reload",
            ]
        );
        let calls_before = service.calls().len();
        let again = uninstall(&sandbox, &service, port);
        assert!(again.status.success(), "{}", text(&again));
        assert_eq!(
            service.calls().len(),
            calls_before,
            "a second uninstall called systemctl"
        );
    }

    /// Codex skips hooks the user has not trusted in `/hooks`. A `setup` that
    /// wired Codex while Codex reports its hooks untrusted names the `/hooks`
    /// approval instead of claiming everything is set up, and still exits 0:
    /// only the user can grant that trust. Codex's `hooks/list` answer comes
    /// from a fake `codex` first on `PATH`; the operator's Codex never runs.
    #[tokio::test]
    async fn setup_names_pending_codex_hook_trust_instead_of_claiming_ready() {
        use std::os::unix::fs::PermissionsExt as _;
        let sandbox = Sandbox::new();
        let service = Service::new(&sandbox.root);
        let hooks = sandbox.home.join(".codex/hooks.json");
        let reply = json!({"jsonrpc": "2.0", "id": 2, "result": {"data": [{"hooks": [{
            "eventName": "sessionStart", "trustStatus": "untrusted", "sourcePath": hooks,
            "command": "ai-memory hook --event session-start --agent codex"
        }]}]}})
        .to_string();
        let fake = service.bin.join("codex");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\nwhile IFS= read -r line; do\ncase \"$line\" in\n*'\"id\":2'*) \
                 printf '%s\\n' '{reply}' ;;\nesac\ndone\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let port = crate::e2e_support::free_port();

        let out = setup(&sandbox, &service, port, &["--yes", "--agent", "codex"]);
        assert!(out.status.success(), "{}", text(&out));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            std::fs::read_to_string(&hooks)
                .unwrap()
                .contains("ai-memory"),
            "{}",
            text(&out)
        );
        assert!(!stdout.contains("ai-memory is set up."), "{}", text(&out));
        assert!(
            stdout.contains("Codex has not approved its 1 ai-memory hook(s)")
                && stdout.contains("sessionStart (untrusted)")
                && stdout.contains("run /hooks"),
            "{}",
            text(&out)
        );
    }

    fn uninstall(sandbox: &Sandbox, service: &Service, port: u16) -> Output {
        let mut cmd = sandbox.cmd(Some(port));
        service.apply(&mut cmd);
        cmd.args(["uninstall", "--apply", "--yes"])
            .output()
            .unwrap()
    }

    /// Adversarial control: a unit at the same path that `setup` did not
    /// write (the packaged one copied by hand), or one edited after `setup`
    /// wrote it, is neither stopped nor deleted, and uninstall says so.
    #[test]
    fn uninstall_leaves_a_unit_it_did_not_write() {
        let sandbox = Sandbox::new();
        let service = Service::new(&sandbox.root);
        let unit_path = sandbox.home.join(".config/systemd/user/ai-memory.service");
        std::fs::create_dir_all(unit_path.parent().unwrap()).unwrap();
        let foreign = include_str!("../../../../packaging/systemd/ai-memory-user.service");
        std::fs::write(&unit_path, foreign).unwrap();
        let port = crate::e2e_support::free_port();

        let out = uninstall(&sandbox, &service, port);
        assert!(out.status.success(), "{}", text(&out));
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("did not write it"),
            "{}",
            text(&out)
        );
        assert_eq!(std::fs::read_to_string(&unit_path).unwrap(), foreign);
        assert!(service.calls().is_empty(), "{:?}", service.calls());
    }
}
