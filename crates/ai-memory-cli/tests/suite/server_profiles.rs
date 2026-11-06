//! Server profiles (#992) driven through the shipped binary: `ai-memory
//! server` writes the registry that `ai-memory hook` then routes by. The unit
//! tests cover each piece in-process; this checks the CLI wiring between them
//! — argument parsing, `--data-dir` resolution, stdin token entry, and the
//! JSON contract of `--check-capture` — without any network: no server is
//! contacted, and the profile URLs here never resolve.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use super::e2e_support::{ServerGuard, free_port, hermetic};

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
const DEFAULT_SERVER: &str = "https://default.example";
const PROFILE_SERVER: &str = "https://b.example";
const SECRET: &str = "SECRET-TOKEN-B";

/// A temp `$HOME` with the repository and data dir inside it, so every marker
/// walk stops at the sandbox and the agent config paths stay out of the real
/// profile.
struct Sandbox {
    _home: tempfile::TempDir,
    home: PathBuf,
    data_dir: PathBuf,
    repo: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_path_buf();
        let repo = home.join("work").join("team-b").join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        for dir in [
            ".config",
            ".local/share",
            "AppData/Roaming",
            "AppData/Local",
        ] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        Self {
            data_dir: home.join(".ai-memory-data"),
            _home: tmp,
            home,
            repo,
        }
    }

    fn root(&self) -> String {
        self.home
            .join("work")
            .join("team-b")
            .to_string_lossy()
            .into_owned()
    }

    fn write_marker(&self, body: &str) {
        std::fs::write(self.repo.join(".ai-memory.toml"), body).unwrap();
    }

    fn cmd(&self) -> Command {
        let mut cmd = hermetic(BIN);
        cmd.env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("APPDATA", self.home.join("AppData/Roaming"))
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("AI_MEMORY_HOME", &self.home)
            .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
            .env("RUST_LOG", "off")
            .arg("--data-dir")
            .arg(&self.data_dir);
        cmd
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> Output {
        let mut child = self
            .cmd()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn `{}`: {e}", args.join(" ")));
        let mut pipe = child.stdin.take().expect("stdin");
        if let Some(input) = stdin {
            pipe.write_all(input.as_bytes()).expect("write stdin");
        }
        drop(pipe);
        child.wait_with_output().expect("wait")
    }

    fn run_ok(&self, args: &[&str], stdin: Option<&str>) -> String {
        let out = self.run(args, stdin);
        assert!(
            out.status.success(),
            "`{}` failed: {}\nstdout: {}\nstderr: {}",
            args.join(" "),
            out.status,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        );
        String::from_utf8(out.stdout).expect("stdout utf8")
    }

    fn hook(&self, event: &str, check_capture: bool) -> Output {
        self.hook_at(&self.repo, DEFAULT_SERVER, None, event, check_capture)
    }

    fn hook_at(
        &self,
        cwd: &Path,
        server_url: &str,
        auth_token: Option<&str>,
        event: &str,
        check_capture: bool,
    ) -> Output {
        let payload = serde_json::json!({
            "session_id": format!("s-{}", cwd.file_name().unwrap().to_string_lossy()),
            "cwd": cwd,
            "prompt": "hello",
        })
        .to_string();
        let mut args = vec![
            "hook",
            "--event",
            event,
            "--agent",
            "claude-code",
            "--server-url",
            server_url,
        ];
        if let Some(token) = auth_token {
            args.extend(["--auth-token", token]);
        }
        if check_capture {
            args.push("--check-capture");
        }
        self.run(&args, Some(&payload))
    }

    fn check_capture(&self) -> serde_json::Value {
        let out = self.hook("user-prompt-submit", true);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).expect("check-capture json")
    }

    fn spooled_entries(&self) -> Vec<serde_json::Value> {
        let Ok(dir) = std::fs::read_dir(self.data_dir.join("hook-spool")) else {
            return Vec::new();
        };
        dir.flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .map(|e| serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap())
            .collect()
    }

    fn register(&self) {
        let root = self.root();
        self.run_ok(
            &[
                "server",
                "add",
                "team-b",
                "--url",
                PROFILE_SERVER,
                "--root",
                &root,
                "--auth-token-stdin",
            ],
            Some(&format!("{SECRET}\n")),
        );
    }
}

fn token_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("auth-tokens")
}

/// The whole path through the binary: register with a token on stdin, the
/// listing shows the profile without the secret, and a hook in the routed
/// repository spools to the profile's server with the profile's token.
#[test]
fn a_routed_repository_spools_to_its_profile_through_the_binary() {
    let sb = Sandbox::new();
    sb.register();

    let listing = sb.run_ok(&["server", "list", "--json"], None);
    let rows: serde_json::Value = serde_json::from_str(&listing).expect("list json");
    assert_eq!(rows[0]["name"], "team-b");
    assert_eq!(rows[0]["url"], PROFILE_SERVER);
    assert_eq!(rows[0]["token"], "stored");
    assert!(!listing.contains(SECRET), "{listing}");
    let human = sb.run_ok(&["server", "list"], None);
    assert!(!human.contains(SECRET), "{human}");

    sb.write_marker("workspace = \"team-b\"\nserver = \"team-b\"\n");
    let check = sb.check_capture();
    assert_eq!(check["server_resolution"], "marker");
    assert_eq!(check["server_profile"], "team-b");
    assert!(!check.to_string().contains(SECRET));
    assert!(!check.to_string().contains("example"), "no URL: {check}");

    let out = sb.hook("user-prompt-submit", false);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"{}\n");
    let entries = sb.spooled_entries();
    assert_eq!(entries.len(), 1, "{entries:?}");
    let url = entries[0]["url"].as_str().unwrap();
    assert!(url.starts_with(&format!("{PROFILE_SERVER}/hook?")), "{url}");
    assert!(url.contains("workspace=team-b"), "{url}");
    assert_eq!(entries[0]["token"], SECRET);
    assert_eq!(entries[0]["profile"], "team-b");
}

/// Without a `server` key the install default is used and the spooled entry
/// has no profile field at all.
#[test]
fn an_unrouted_repository_keeps_the_install_default_through_the_binary() {
    let sb = Sandbox::new();
    sb.register();
    sb.write_marker("workspace = \"team-b\"\n");

    assert_eq!(sb.check_capture()["server_resolution"], "install-default");
    let out = sb.hook("user-prompt-submit", false);
    assert!(out.status.success());
    let entries = sb.spooled_entries();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0]["url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("{DEFAULT_SERVER}/hook?"))
    );
    assert!(entries[0].get("profile").is_none(), "{:?}", entries[0]);
    assert!(entries[0].get("token").is_none(), "{:?}", entries[0]);
}

/// A selection that does not resolve emits nothing: no spool entry, a
/// warning on stderr that names the profile but not a URL or token, and a
/// `--check-capture` that reports the reason.
#[test]
fn an_unknown_profile_drops_the_event_through_the_binary() {
    let sb = Sandbox::new();
    sb.register();
    sb.write_marker("server = \"nobody\"\n");

    assert_eq!(
        sb.check_capture()["server_resolution"],
        "rejected-unknown-profile"
    );
    let out = sb.hook("user-prompt-submit", false);
    assert!(out.status.success());
    assert_eq!(out.stdout, b"{}\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("`nobody`"), "{stderr}");
    assert!(stderr.contains("rejected-unknown-profile"), "{stderr}");
    assert!(
        !stderr.contains("example") && !stderr.contains(SECRET),
        "{stderr}"
    );
    assert!(sb.spooled_entries().is_empty());
}

/// A real `ai-memory serve` child on a free loopback port with its own data
/// dir and root token, hermetic like the other e2e suites (no embedder, no
/// wiki watcher).
struct RealServer {
    _guard: ServerGuard,
    _data: tempfile::TempDir,
    base: String,
    token: &'static str,
}

impl RealServer {
    async fn start(token: &'static str) -> Self {
        let data = tempfile::tempdir().unwrap();
        let port = free_port();
        let base = format!("http://127.0.0.1:{port}");
        let guard = ServerGuard(
            hermetic(BIN)
                .args([
                    "serve",
                    "--transport",
                    "http",
                    "--bind",
                    &format!("127.0.0.1:{port}"),
                    "--no-watcher",
                ])
                .env("AI_MEMORY_DATA_DIR", data.path())
                .env("AI_MEMORY_HOME", data.path())
                .env("AI_MEMORY_AUTH_TOKEN", token)
                .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
                .env("RUST_LOG", "off")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn serve"),
        );
        let client = reqwest::Client::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if client
                .get(format!("{base}/mcp"))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "server on {base} never became reachable"
            );
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        Self {
            _guard: guard,
            _data: data,
            base,
            token,
        }
    }

    /// The server's session count for one scope as seen with `token`, or the
    /// HTTP status when the server refuses the request. A scope the server
    /// has never written (404) counts as zero.
    async fn sessions(
        &self,
        client: &reqwest::Client,
        token: &str,
        workspace: &str,
        project: &str,
    ) -> Result<u64, reqwest::StatusCode> {
        let resp = client
            .get(format!("{}/admin/sessions/by-agent", self.base))
            .bearer_auth(token)
            .query(&[("workspace", workspace), ("project", project)])
            .send()
            .await
            .expect("by-agent request");
        match resp.status() {
            s if s.is_success() => {
                let body: serde_json::Value = resp.json().await.expect("by-agent json");
                Ok(body["by_agent"]
                    .as_array()
                    .map(|agents| agents.iter().filter_map(|a| a["sessions"].as_u64()).sum())
                    .unwrap_or(0))
            }
            reqwest::StatusCode::NOT_FOUND => Ok(0),
            other => Err(other),
        }
    }

    /// Poll until the scope shows `want` sessions: `/hook/batch` answers as
    /// soon as the events are queued, and the writer admits them shortly
    /// after.
    async fn settled_sessions(
        &self,
        client: &reqwest::Client,
        workspace: &str,
        project: &str,
        want: u64,
    ) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let count = self
                .sessions(client, self.token, workspace, project)
                .await
                .unwrap_or_else(|status| panic!("{} refused its own token: {status}", self.base));
            if count == want || Instant::now() >= deadline {
                return count;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// The manual two-server check as a test: two real servers, each with its own
/// root token, and one client install whose install default is server A. A
/// repository routed to profile `b` ends up as a session on server B only,
/// an unrouted one on server A only, after the spool is drained by the same
/// `hook-drain` an installed hook would spawn. Each server refuses the other's
/// token, so a swapped credential would have been rejected at delivery.
#[tokio::test]
async fn two_real_servers_each_receive_only_their_own_repository() {
    let sb = Sandbox::new();
    let server_a = RealServer::start("TOKEN-A").await;
    let server_b = RealServer::start("TOKEN-B").await;
    let repo_a = sb.home.join("work").join("alpha");
    let repo_b = sb.home.join("work").join("beta");
    std::fs::create_dir_all(&repo_a).unwrap();
    std::fs::create_dir_all(&repo_b).unwrap();
    std::fs::write(
        repo_a.join(".ai-memory.toml"),
        "workspace = \"alpha-ws\"\nproject = \"alpha\"\n",
    )
    .unwrap();
    std::fs::write(
        repo_b.join(".ai-memory.toml"),
        "workspace = \"beta-ws\"\nproject = \"beta\"\nserver = \"b\"\n",
    )
    .unwrap();
    let root_b = repo_b.to_string_lossy().into_owned();
    sb.run_ok(
        &[
            "server",
            "add",
            "b",
            "--url",
            &server_b.base,
            "--root",
            &root_b,
            "--auth-token-stdin",
        ],
        Some("TOKEN-B\n"),
    );

    for repo in [&repo_a, &repo_b] {
        let out = sb.hook_at(
            repo,
            &server_a.base,
            Some("TOKEN-A"),
            "user-prompt-submit",
            false,
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(sb.spooled_entries().len(), 2);
    sb.run_ok(&["hook-drain"], None);
    assert!(
        sb.spooled_entries().is_empty(),
        "the drain delivered both entries"
    );

    let client = reqwest::Client::new();
    assert_eq!(
        server_a
            .settled_sessions(&client, "alpha-ws", "alpha", 1)
            .await,
        1
    );
    assert_eq!(
        server_b
            .settled_sessions(&client, "beta-ws", "beta", 1)
            .await,
        1
    );
    assert_eq!(
        server_a
            .sessions(&client, "TOKEN-A", "beta-ws", "beta")
            .await,
        Ok(0),
        "the routed repository must not reach the install default"
    );
    assert_eq!(
        server_b
            .sessions(&client, "TOKEN-B", "alpha-ws", "alpha")
            .await,
        Ok(0),
        "the unrouted repository must not reach the profile's server"
    );
    assert_eq!(
        server_b
            .sessions(&client, "TOKEN-A", "beta-ws", "beta")
            .await,
        Err(reqwest::StatusCode::UNAUTHORIZED),
        "control: server B refuses server A's token, so a swapped bearer could not have delivered"
    );
}

/// `uninstall` removes the hooks that read the profile tokens, so it removes
/// the tokens too — but keeps the registry, exactly as it keeps `config.toml`.
#[test]
fn uninstall_removes_profile_tokens_and_keeps_the_registry() {
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.home.join(".claude")).unwrap();
    sb.run_ok(
        &["install-hooks", "--agent", "claude-code", "--apply"],
        None,
    );
    sb.register();
    assert!(token_dir(&sb.data_dir).join("team-b").is_file());

    sb.run_ok(&["uninstall", "--apply", "--only", "hooks", "--yes"], None);

    assert!(
        !token_dir(&sb.data_dir).exists(),
        "tokens must not outlive the hooks"
    );
    assert!(sb.data_dir.join("servers.toml").is_file());
    let listing = sb.run_ok(&["server", "list", "--json"], None);
    let rows: serde_json::Value = serde_json::from_str(&listing).unwrap();
    assert_eq!(rows[0]["token"], "missing", "{listing}");
}

/// The install's own server for the #2016 tests: a dead endpoint in
/// `config.toml`, so a command that ignored the marker's profile fails to
/// connect instead of reaching whatever listens on the default port.
fn point_config_at_a_dead_server(sb: &Sandbox) -> String {
    let dead = ai_memory_test_support::dead_http_endpoint();
    std::fs::create_dir_all(&sb.data_dir).unwrap();
    std::fs::write(
        sb.data_dir.join("config.toml"),
        format!("server_url = \"{dead}\"\n"),
    )
    .unwrap();
    dead
}

fn in_repo(sb: &Sandbox, args: &[&str], env: &[(&str, &str)]) -> Output {
    in_dir(sb, &sb.repo, args, env)
}

/// Run a command in `dir` the way a shell does: `PWD` holds the logical
/// path, while the process's own cwd is the physical one (macOS reports a
/// tempdir under `/var` as `/private/var`).
fn in_dir(sb: &Sandbox, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = sb.cmd();
    cmd.current_dir(dir)
        .env("PWD", dir)
        .args(args)
        .stdin(Stdio::null());
    for (key, value) in env {
        cmd.env(key, value);
    }
    cmd.output()
        .unwrap_or_else(|e| panic!("spawn `{}`: {e}", args.join(" ")))
}

/// #2016: every command that works on this checkout reaches the server its
/// marker selects, with the profile's stored token, as the hooks do. The
/// install's server is dead and server B refuses any other token, so each
/// command that succeeds went to B with B's token. `AI_MEMORY_SERVER_URL`
/// still outranks the marker.
#[tokio::test]
async fn checkout_commands_reach_the_marker_profile_server() {
    let sb = Sandbox::new();
    let dead = point_config_at_a_dead_server(&sb);
    let server_b = RealServer::start("TOKEN-B").await;
    sb.run_ok(
        &[
            "server",
            "add",
            "team-b",
            "--url",
            &server_b.base,
            "--root",
            &sb.root(),
            "--auth-token-stdin",
        ],
        Some("TOKEN-B\n"),
    );
    sb.write_marker("workspace = \"team-b\"\nproject = \"repo\"\nserver = \"team-b\"\n");

    let commands: &[&[&str]] = &[
        &[
            "write-page",
            "--path",
            "notes/routed.md",
            "--body",
            "# Routed\n\nprofile-routed body",
        ],
        &["read-page", "--path", "notes/routed.md"],
        &["search", "profile-routed"],
        &["recent"],
        &["handoffs"],
        &["message", "list"],
        &["workstreams"],
        &["pending-writes", "list"],
        &["doctor"],
    ];
    for args in commands {
        let out = in_repo(&sb, args, &[]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "`{}` did not reach the profile's server: {stderr}",
            args.join(" ")
        );
        assert!(!stderr.contains(&dead), "`{}`: {stderr}", args.join(" "));
    }
    let read = in_repo(&sb, &["read-page", "--path", "notes/routed.md"], &[]);
    assert!(
        String::from_utf8_lossy(&read.stdout).contains("profile-routed body"),
        "the page written through the profile is on server B"
    );

    // Control: an explicit AI_MEMORY_SERVER_URL wins over the marker, so the
    // same command now dials the dead server.
    let out = in_repo(&sb, &["recent"], &[("AI_MEMORY_SERVER_URL", &dead)]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&dead), "{stderr}");
}

/// #2016: a marker selecting a profile that does not resolve fails the
/// command closed with the hook's reason, and `run --require-server` probes
/// the profile's server rather than the install's.
#[test]
fn checkout_commands_fail_closed_on_a_refused_profile_and_run_probes_the_profile() {
    let sb = Sandbox::new();
    let dead = point_config_at_a_dead_server(&sb);
    sb.write_marker("server = \"nobody\"\n");
    for args in [&["recent"][..], &["workstreams"], &["doctor"]] {
        let out = in_repo(&sb, args, &[]);
        assert!(!out.status.success(), "`{}`", args.join(" "));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("rejected-unknown-profile"), "{stderr}");
        assert!(
            !stderr.contains(&dead),
            "nothing dialed the install's server: {stderr}"
        );
    }

    let profile_server = ai_memory_test_support::dead_http_endpoint();
    let root = sb.root();
    sb.run_ok(
        &[
            "server",
            "add",
            "team-b",
            "--url",
            &profile_server,
            "--root",
            &root,
            "--auth-token-stdin",
        ],
        Some("TOKEN-B\n"),
    );
    sb.write_marker("server = \"team-b\"\n");
    // The probe runs before the harness starts; a shebang script stands in
    // for it, so this half needs Unix.
    if !cfg!(unix) {
        return;
    }
    let harness = sb.home.join("fake-claude");
    std::fs::write(&harness, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&harness, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = in_repo(
        &sb,
        &[
            "run",
            "--require-server",
            "--no-jail",
            "--executable",
            &harness.to_string_lossy(),
            "claude",
        ],
        &[],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&profile_server), "{stderr}");
    assert!(!stderr.contains(&dead), "{stderr}");
}

/// #2016 on any Unix: the profile's root is registered through a symlink, as
/// a harness reports the checkout, while the command's own cwd is the
/// physical directory. The command follows the hooks and routes to the
/// profile; a `PWD` that names another directory is ignored, so the physical
/// path is matched and refused, as is a missing `PWD`.
#[cfg(unix)]
#[test]
fn checkout_commands_match_profile_roots_through_a_symlinked_checkout() {
    let sb = Sandbox::new();
    let dead = point_config_at_a_dead_server(&sb);
    let physical = sb.home.join("physical").join("team-b");
    std::fs::create_dir_all(physical.join("repo")).unwrap();
    std::fs::write(
        physical.join("repo").join(".ai-memory.toml"),
        "server = \"team-b\"\n",
    )
    .unwrap();
    let linked = sb.home.join("work").join("linked");
    std::os::unix::fs::symlink(&physical, &linked).unwrap();
    let repo = linked.join("repo");
    let profile_server = ai_memory_test_support::dead_http_endpoint();
    let root = linked.to_string_lossy().into_owned();
    sb.run_ok(
        &[
            "server",
            "add",
            "team-b",
            "--url",
            &profile_server,
            "--root",
            &root,
            "--auth-token-stdin",
        ],
        Some("TOKEN-B\n"),
    );

    // The hook, given the harness's logical cwd, routes to the profile.
    let check = sb.hook_at(&repo, DEFAULT_SERVER, None, "user-prompt-submit", true);
    let check: serde_json::Value = serde_json::from_slice(&check.stdout).unwrap();
    assert_eq!(check["server_resolution"], "marker");

    // So does the command: it dials the profile's (dead) server, never the
    // install's, and is not refused.
    let out = in_dir(&sb, &repo, &["recent"], &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(&profile_server), "{stderr}");
    assert!(!stderr.contains("rejected-outside-roots"), "{stderr}");
    assert!(!stderr.contains(&dead), "{stderr}");

    // A PWD naming another directory, or none, leaves the physical path,
    // which lies outside the root: refused, nothing sent anywhere.
    for pwd in [Some(sb.home.as_path()), None] {
        let mut cmd = sb.cmd();
        cmd.current_dir(&repo).arg("recent").stdin(Stdio::null());
        match pwd {
            Some(pwd) => cmd.env("PWD", pwd),
            None => cmd.env_remove("PWD"),
        };
        let out = cmd.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success());
        assert!(
            stderr.contains("rejected-outside-roots"),
            "{pwd:?}: {stderr}"
        );
        assert!(
            !stderr.contains(&dead) && !stderr.contains(&profile_server),
            "{stderr}"
        );
    }
}
