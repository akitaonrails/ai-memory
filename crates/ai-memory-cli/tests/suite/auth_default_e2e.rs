//! Auth on by default for new installs, through the shipped binary
//! (`docs/security-boundaries.md` row 12).
//!
//! A fresh `ai-memory init` mints a root bearer into
//! `<data_dir>/auth-tokens/default` and records `[auth].bearer_token_file`.
//! The loopback server it configures must then refuse every unauthenticated
//! machine, API and admin route while `/healthz` stays open, and the
//! first-party clients must keep working with no token in their environment:
//! the native hook (through the hook credential files `init` seeds), thin CLI
//! commands (through config), and `ai-memory mcp-bridge` (which reads the
//! file itself). The token must appear in no harness config and no process
//! command line.
//!
//! The control is an install that predates the default: same binary, no token
//! file, and it must behave exactly as before until `auth enable` opts it in.

/// Spawns servers: seconds, not milliseconds, so the slow tier.
// Unix only: Windows resolves the harness app-data folders (Zed, OpenClaw)
// through known-folder lookups that ignore the sandbox's HOME/USERPROFILE,
// so these flows cannot run hermetically there. The unit tests cover the
// Windows rendering paths.
#[cfg(unix)]
mod slow {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use reqwest::StatusCode;
    use serde_json::{Value, json};

    use crate::e2e_support::{ServerGuard, hermetic, start_serve};

    const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
    const WORKSPACE: &str = "authdef-ws";
    const PROJECT: &str = "authdef-project";

    /// A temp home and a data dir beside it (not inside it, so a scan of the
    /// home is a scan of harness configs only), plus a repository whose
    /// marker pins the scope the hook writes to.
    struct Install {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        home: PathBuf,
        data_dir: PathBuf,
        repo: PathBuf,
    }

    impl Install {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().to_path_buf();
            let home = root.join("home");
            let repo = home.join("work").join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            for dir in [".config", ".local/share", ".claude"] {
                std::fs::create_dir_all(home.join(dir)).unwrap();
            }
            std::fs::write(
                repo.join(".ai-memory.toml"),
                format!("workspace = \"{WORKSPACE}\"\nproject = \"{PROJECT}\"\n"),
            )
            .unwrap();
            Self {
                data_dir: root.join("data"),
                _tmp: tmp,
                root,
                home,
                repo,
            }
        }

        /// The operator's shell: no `AI_MEMORY_AUTH_TOKEN`, ever.
        fn cmd(&self) -> Command {
            let mut cmd = hermetic(BIN);
            cmd.env("HOME", &self.home)
                .env("USERPROFILE", &self.home)
                .env("XDG_CONFIG_HOME", self.home.join(".config"))
                .env("XDG_DATA_HOME", self.home.join(".local/share"))
                .env("AI_MEMORY_HOME", &self.home)
                .env("AI_MEMORY_DATA_DIR", &self.data_dir)
                .env("AI_MEMORY_EMBEDDING_PROVIDER", "none")
                .env("AI_MEMORY_BACKFILL_ON_START", "false")
                .env("AI_MEMORY_AUTO_IMPROVE__SCHEDULER__ENABLED", "false")
                .env("RUST_LOG", "off")
                // A developer (or agent) running these tests from Claude Code
                // has its session id exported; a child must not inherit it.
                .env_remove("CLAUDE_CODE_SESSION_ID")
                .env_remove("MUSE_SESSION_ID")
                .current_dir(&self.home);
            cmd
        }

        fn run(&self, args: &[&str], base: Option<&str>, stdin: Option<&str>) -> Output {
            let mut cmd = self.cmd();
            if let Some(base) = base {
                cmd.env("AI_MEMORY_SERVER_URL", base);
            }
            let mut child = cmd
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

        fn run_ok(&self, args: &[&str], base: Option<&str>) -> String {
            let out = self.run(args, base, None);
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

        async fn serve(&self, client: &reqwest::Client, log: &str) -> (ServerGuard, String) {
            self.serve_with(client, log, &[]).await
        }

        async fn serve_with(
            &self,
            client: &reqwest::Client,
            log: &str,
            global: &[&str],
        ) -> (ServerGuard, String) {
            start_serve(client, &self.root.join(log), |port| {
                let mut cmd = self.cmd();
                cmd.env_remove("RUST_LOG")
                    .args(global)
                    .args(["serve", "--transport", "http", "--enable-web", "--bind"])
                    .arg(format!("127.0.0.1:{port}"))
                    .arg("--no-watcher");
                cmd
            })
            .await
        }

        fn root_token(&self) -> Option<String> {
            let text =
                std::fs::read_to_string(self.data_dir.join("auth-tokens").join("default")).ok()?;
            let token = text.trim().to_owned();
            (!token.is_empty()).then_some(token)
        }

        /// One prompt through the native hook, as an installed hook would run
        /// it: no `--auth-token`, no token in the environment.
        fn hook_prompt(&self, base: &str, session: &str) {
            let payload = json!({
                "session_id": session,
                "cwd": self.repo,
                "prompt": "authdef hook prompt",
            })
            .to_string();
            let out = self.run(
                &[
                    "hook",
                    "--event",
                    "user-prompt-submit",
                    "--agent",
                    "claude-code",
                    "--server-url",
                    base,
                ],
                None,
                Some(&payload),
            );
            assert!(
                out.status.success(),
                "hook failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            self.run_ok(&["hook-drain"], None);
        }

        /// Every file under `dir` that contains `needle`, skipping `allowed`.
        fn files_containing(dir: &Path, needle: &str, allowed: &[PathBuf]) -> Vec<PathBuf> {
            let mut hits = Vec::new();
            let mut pending = vec![dir.to_path_buf()];
            while let Some(dir) = pending.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if allowed.iter().any(|a| path.starts_with(a)) {
                        continue;
                    }
                    if path.is_dir() {
                        pending.push(path);
                    } else if let Ok(bytes) = std::fs::read(&path)
                        && bytes
                            .windows(needle.len())
                            .any(|window| window == needle.as_bytes())
                    {
                        hits.push(path);
                    }
                }
            }
            hits
        }

        /// The token may live only in the token file and the two hook
        /// credential files. A harness config, the database, the wiki or a log
        /// carrying it would be a leak.
        fn assert_token_confined(&self, token: &str) {
            let leaks = Self::files_containing(&self.home, token, &[]);
            assert!(leaks.is_empty(), "token in harness/home files: {leaks:?}");
            let allowed = [
                self.data_dir.join("auth-tokens").join("default"),
                self.data_dir.join("auth-token"),
                self.data_dir.join("auth-header"),
                // A spooled event records the bearer it will be replayed with;
                // the spool is drained before every scan, and is 0700.
                self.data_dir.join("hook-spool"),
            ];
            let leaks = Self::files_containing(&self.data_dir, token, &allowed);
            assert!(
                leaks.is_empty(),
                "token copied inside the data dir: {leaks:?}"
            );
        }
    }

    async fn status_of(
        client: &reqwest::Client,
        method: reqwest::Method,
        url: String,
        body: Option<Value>,
        token: Option<&str>,
    ) -> StatusCode {
        let mut req = client
            .request(method, url)
            .header("accept", "application/json, text/event-stream");
        if let Some(body) = body {
            req = req.json(&body);
        }
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        req.send().await.expect("request").status()
    }

    /// The routes a token must guard, each sent the way a first-party client
    /// would send it (JSON, no browser headers), so a refusal can only be the
    /// bearer check.
    async fn route_statuses(
        client: &reqwest::Client,
        base: &str,
        token: Option<&str>,
    ) -> Vec<(&'static str, StatusCode)> {
        use reqwest::Method;
        let initialize = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18", "capabilities": {},
                "clientInfo": {"name": "authdef-e2e", "version": "0"}
            }
        });
        let hook = json!({"session_id": "authdef-route", "cwd": "/tmp", "prompt": "x"});
        vec![
            (
                "/mcp",
                status_of(client, Method::POST, format!("{base}/mcp"), Some(initialize), token)
                    .await,
            ),
            (
                "/hook",
                status_of(
                    client,
                    Method::POST,
                    format!(
                        "{base}/hook?event=user-prompt-submit&agent=claude-code&workspace={WORKSPACE}&project=authdef-route-probes"
                    ),
                    Some(hook),
                    token,
                )
                .await,
            ),
            (
                "/api/v1/projects",
                status_of(client, Method::GET, format!("{base}/api/v1/projects"), None, token)
                    .await,
            ),
            (
                "/admin/status",
                status_of(client, Method::GET, format!("{base}/admin/status"), None, token).await,
            ),
        ]
    }

    async fn healthz(client: &reqwest::Client, base: &str) -> StatusCode {
        client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .expect("healthz")
            .status()
    }

    async fn settled_sessions(client: &reqwest::Client, base: &str, token: &str, want: u64) -> u64 {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let resp = client
                .get(format!("{base}/admin/sessions/by-agent"))
                .bearer_auth(token)
                .query(&[("workspace", WORKSPACE), ("project", PROJECT)])
                .send()
                .await
                .expect("by-agent");
            let count = if resp.status().is_success() {
                let body: Value = resp.json().await.expect("by-agent json");
                body["by_agent"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x["sessions"].as_u64()).sum())
                    .unwrap_or(0)
            } else {
                assert_eq!(resp.status(), StatusCode::NOT_FOUND, "by-agent refused");
                0
            };
            if count >= want || Instant::now() >= deadline {
                return count;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[cfg(target_os = "linux")]
    fn cmdline(pid: u32) -> String {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', " "))
            .unwrap_or_default()
    }

    /// Drive `ai-memory mcp-bridge` over stdio the way Claude Code does:
    /// `initialize`, `notifications/initialized`, `tools/list`. Returns the
    /// `tools/list` reply. `check` runs while the bridge is alive.
    fn bridge_tools_list(install: &Install, base: &str, check: impl FnOnce(u32)) -> Value {
        bridge_exchange(install, BridgeMode::SessionAware, base, None, check).0
    }

    /// How the bridge is launched, as the entry `install-mcp` writes runs it.
    #[derive(Clone, Copy)]
    enum BridgeMode {
        /// Claude Code's entry: forwards `CLAUDE_CODE_SESSION_ID`.
        SessionAware,
        /// Every other client's entry: `--no-session-id`, no id in the env.
        SessionLess,
    }

    /// `initialize`, `notifications/initialized`, `tools/list` and, when
    /// given, one `tools/call`, through the bridge over stdio. Returns the
    /// `tools/list` and `tools/call` replies; `check` runs while it is alive.
    fn bridge_exchange(
        install: &Install,
        mode: BridgeMode,
        base: &str,
        call: Option<Value>,
        check: impl FnOnce(u32),
    ) -> (Value, Option<Value>) {
        bridge_exchange_in(install, &install.home, mode, base, call, check)
    }

    /// [`bridge_exchange`] with the bridge launched from `dir`, as a client
    /// starts it from the repository it works in.
    fn bridge_exchange_in(
        install: &Install,
        dir: &Path,
        mode: BridgeMode,
        base: &str,
        call: Option<Value>,
        check: impl FnOnce(u32),
    ) -> (Value, Option<Value>) {
        let mut cmd = install.cmd();
        cmd.current_dir(dir);
        match mode {
            BridgeMode::SessionAware => {
                cmd.args(["mcp-bridge", "--server-url", base])
                    .env("CLAUDE_CODE_SESSION_ID", "authdef-bridge-session");
            }
            BridgeMode::SessionLess => {
                // A client started from a Claude Code terminal inherits that
                // session's id; the session-less bridge must work regardless.
                cmd.args(["mcp-bridge", "--no-session-id", "--server-url", base])
                    .env("CLAUDE_CODE_SESSION_ID", "inherited-from-a-claude-terminal");
            }
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn mcp-bridge");
        let mut stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = mpsc::channel::<Value>();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(value) = serde_json::from_str::<Value>(&line)
                    && tx.send(value).is_err()
                {
                    break;
                }
            }
        });
        let send = |stdin: &mut std::process::ChildStdin, value: Value| {
            writeln!(stdin, "{value}").expect("write to bridge");
            stdin.flush().expect("flush bridge stdin");
        };
        let wait_for = |id: u64| -> Value {
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(left) {
                    Ok(value) if value["id"] == id => return value,
                    Ok(_) => {}
                    Err(e) => panic!("bridge gave no reply {id}: {e}"),
                }
            }
        };
        send(
            &mut stdin,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18", "capabilities": {},
                    "clientInfo": {"name": "authdef-e2e", "version": "0"}
                }
            }),
        );
        let init = wait_for(1);
        assert!(init.get("result").is_some(), "bridge initialize: {init}");
        send(
            &mut stdin,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        );
        send(
            &mut stdin,
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
        );
        let tools = wait_for(2);
        let called = call.map(|params| {
            send(
                &mut stdin,
                json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": params}),
            );
            wait_for(3)
        });
        check(child.id());
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        (tools, called)
    }

    #[cfg(target_os = "linux")]
    fn environ(pid: u32) -> String {
        std::fs::read(format!("/proc/{pid}/environ"))
            .map(|bytes| String::from_utf8_lossy(&bytes).replace('\0', "\n"))
            .unwrap_or_default()
    }

    /// The clients `install-mcp --apply` writes a config file for on a
    /// token-file install, by their CLI names. Pi (an extension), DeepSeek
    /// Harness (print-only) and ZCode (HTTP-only, refused) are not among them.
    const WRITTEN_CLIENTS: &[&str] = &[
        "claude-code",
        "codex",
        "opencode2",
        "cursor",
        "claude-desktop",
        "gemini-cli",
        "qwen-code",
        "openclaw",
        "omp",
        "antigravity-cli",
        "zero",
        "devin",
        "grok",
        "kimi-code",
        "kiro-cli",
        "command-code",
        "copilot-cli",
        "droid",
        "swival",
        "vscode-copilot",
        "zed",
        "muse",
        "prime-agent",
    ];

    /// A fresh `init`: every guarded route refuses an unauthenticated
    /// request, `/healthz` answers, and the hook, a CLI command and the MCP
    /// bridge all succeed without a token in their environment. Wiring the
    /// harnesses and running `auth enable` twice leaves the token in no
    /// harness config and no command line.
    #[tokio::test]
    async fn a_fresh_install_requires_the_token_and_its_own_clients_still_work() {
        let install = Install::new();
        install.run_ok(&["init"], None);
        let token = install.root_token().expect("init minted a root bearer");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(install.data_dir.join("auth-tokens").join("default"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the root bearer is owner-only");
        }

        let client = reqwest::Client::new();
        let (server, base) = install.serve(&client, "serve.log").await;

        for (route, status) in route_statuses(&client, &base, None).await {
            assert_eq!(status, StatusCode::UNAUTHORIZED, "unauthenticated {route}");
        }
        assert_eq!(healthz(&client, &base).await, StatusCode::OK);
        // Control: the same requests with the token get past the gate, so the
        // 401s above are the bearer check and not a broken route.
        for (route, status) in route_statuses(&client, &base, Some(&token)).await {
            assert_ne!(status, StatusCode::UNAUTHORIZED, "authenticated {route}");
            assert!(status.is_success(), "authenticated {route}: {status}");
        }

        // The native hook authenticates through the files `init` seeded.
        install.hook_prompt(&base, "authdef-hook-session");
        assert_eq!(settled_sessions(&client, &base, &token, 1).await, 1);

        // A thin CLI command authenticates through config.
        let status = install.run_ok(&["status"], Some(&base));
        assert!(status.contains("auth:         bearer token"), "{status}");

        // The bridge reads the token file itself.
        let tools = bridge_tools_list(&install, &base, |_pid| {
            #[cfg(target_os = "linux")]
            assert!(
                !cmdline(_pid).contains(&token),
                "token on the bridge's command line"
            );
        });
        let listed = tools["result"]["tools"].as_array().map_or(0, Vec::len);
        assert!(listed > 0, "bridge tools/list: {tools}");
        #[cfg(target_os = "linux")]
        assert!(
            !cmdline(server.0.id()).contains(&token),
            "token on the server's command line"
        );

        // Wire the harnesses through the normal install paths, then opt in
        // again: nothing may copy the token into a harness config.
        let hooks_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks");
        let settings = install.home.join(".claude").join("settings.json");
        install.run_ok(
            &[
                "install-hooks",
                "--agent",
                "claude-code",
                "--apply",
                "--hooks-dir",
                hooks_dir.to_str().unwrap(),
                "--config-file",
                settings.to_str().unwrap(),
            ],
            Some(&base),
        );
        for client in WRITTEN_CLIENTS {
            install.run_ok(&["install-mcp", "--client", client, "--apply"], Some(&base));
        }
        let refused = install.run(
            &["install-mcp", "--client", "zcode", "--apply"],
            Some(&base),
            None,
        );
        assert!(
            !refused.status.success(),
            "ZCode, which only takes a literal header, is refused"
        );
        let first = install.run_ok(&["auth", "enable"], Some(&base));
        let config_after_first =
            std::fs::read_to_string(install.data_dir.join("config.toml")).unwrap();
        let claude_after_first =
            std::fs::read_to_string(install.home.join(".claude.json")).unwrap();
        let second = install.run_ok(&["auth", "enable"], Some(&base));
        assert!(first.contains("kept"), "{first}");
        assert!(second.contains("kept"), "{second}");
        assert_eq!(
            std::fs::read_to_string(install.data_dir.join("config.toml")).unwrap(),
            config_after_first
        );
        assert_eq!(
            std::fs::read_to_string(install.home.join(".claude.json")).unwrap(),
            claude_after_first,
            "a second auth enable changes nothing"
        );
        assert_eq!(install.root_token().as_deref(), Some(token.as_str()));
        let claude: Value = serde_json::from_str(&claude_after_first).unwrap();
        assert_eq!(claude["mcpServers"]["ai-memory"]["args"][0], "mcp-bridge");
        for output in [&first, &second, &status] {
            assert!(!output.contains(&token), "token printed: {output}");
        }
        install.assert_token_confined(&token);
        drop(server);
    }

    /// The session-less bridge any non-Claude client runs: against a fresh
    /// auth-on install it lists the tools and answers a `memory_query` with
    /// explicit scope, with the token on neither its command line nor in its
    /// environment. Controls: the session-aware bridge still refuses to start
    /// without `CLAUDE_CODE_SESSION_ID`, and the static `memory_query` reaches
    /// the project the hook wrote.
    #[tokio::test]
    async fn a_session_less_bridge_serves_any_client_without_holding_the_token() {
        let install = Install::new();
        install.run_ok(&["init"], None);
        let token = install.root_token().unwrap();
        let client = reqwest::Client::new();
        let (_server, base) = install.serve(&client, "serve-bridge.log").await;
        install.hook_prompt(&base, "authdef-bridge-hook");
        assert_eq!(settled_sessions(&client, &base, &token, 1).await, 1);

        let query = json!({
            "name": "memory_query",
            "arguments": {"q": "authdef hook prompt", "workspace": WORKSPACE, "project": PROJECT},
        });
        let (tools, called) = bridge_exchange(
            &install,
            BridgeMode::SessionLess,
            &base,
            Some(query),
            |_pid| {
                #[cfg(target_os = "linux")]
                {
                    assert!(!cmdline(_pid).contains(&token), "token in the bridge argv");
                    assert!(!environ(_pid).contains(&token), "token in the bridge env");
                }
            },
        );
        assert!(
            tools["result"]["tools"]
                .as_array()
                .is_some_and(|t| !t.is_empty()),
            "{tools}"
        );
        let called = called.expect("memory_query reply");
        assert!(called.get("error").is_none(), "{called}");
        assert_ne!(called["result"]["isError"], json!(true), "{called}");

        let out = install.run(&["mcp-bridge", "--server-url", &base], None, Some(""));
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("CLAUDE_CODE_SESSION_ID is missing"),
            "the session-aware mode keeps failing closed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Muse's bridge reads only MUSE_SESSION_ID: an inherited Claude id
        // does not stand in for it.
        let mut muse = install.cmd();
        let out = muse
            .args(["mcp-bridge", "--agent", "muse", "--server-url", &base])
            .env("CLAUDE_CODE_SESSION_ID", "inherited-from-a-claude-terminal")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("MUSE_SESSION_ID is missing"),
            "the Muse bridge fails closed without its own id: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The text of a `tools/call` reply, failing on an error reply.
    fn tool_text(reply: &Value) -> String {
        assert!(reply.get("error").is_none(), "{reply}");
        assert_ne!(reply["result"]["isError"], json!(true), "{reply}");
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    /// #1979, the reporter's case against the shipped `serve`: the hooks make
    /// one repository the active project, and a session-less bridge launched
    /// in another marked repository sends its unscoped calls to that other
    /// repository, a write included. Its explicit calls keep their scope, a
    /// bridge launched in the first repository cannot read the second's page
    /// without naming it, and the session-aware bridge uses the launch
    /// repository only until its session's pointer is published (#2005).
    #[tokio::test]
    async fn a_session_less_bridge_scopes_unscoped_calls_to_its_launch_repository() {
        const NOTES: &str = "authdef-notes";
        let install = Install::new();
        install.run_ok(&["init"], None);
        let token = install.root_token().unwrap();
        let notes = install.home.join("work").join("notes");
        std::fs::create_dir_all(&notes).unwrap();
        std::fs::write(
            notes.join(".ai-memory.toml"),
            format!("workspace = \"{WORKSPACE}\"\nproject = \"{NOTES}\"\n"),
        )
        .unwrap();
        let client = reqwest::Client::new();
        let (_server, base) = install.serve(&client, "serve-launch-scope.log").await;
        install.hook_prompt(&base, "authdef-launch-scope-hook");
        assert_eq!(settled_sessions(&client, &base, &token, 1).await, 1);

        let session_less = |dir: &Path, call: Value| {
            let (_, reply) = bridge_exchange_in(
                &install,
                dir,
                BridgeMode::SessionLess,
                &base,
                Some(call),
                |_| {},
            );
            tool_text(&reply.expect("tools/call reply"))
        };
        let write = json!({
            "name": "memory_write_page",
            "arguments": {
                "path": "notes/launch-scope.md",
                "title": "Launch scope",
                "body": "The zircon ledger is recorded only in the notes repository.",
            },
        });
        session_less(&notes, write);
        let unscoped_query = json!({"name": "memory_query", "arguments": {"q": "zircon ledger"}});
        let scoped_query = |project: &str| {
            json!({
                "name": "memory_query",
                "arguments": {"q": "zircon ledger", "workspace": WORKSPACE, "project": project},
            })
        };

        // The unscoped write landed in the launch repository's project.
        assert!(
            session_less(&install.home, scoped_query(NOTES)).contains("launch-scope"),
            "the page is in {NOTES}"
        );
        assert!(
            !session_less(&install.home, scoped_query(PROJECT)).contains("launch-scope"),
            "and not in the hook's active project"
        );
        assert!(
            session_less(&notes, unscoped_query.clone()).contains("launch-scope"),
            "an unscoped query from the notes checkout answers from notes"
        );
        // Adversarial: launched elsewhere, an unscoped call does not reach it,
        // and an explicit scope from the notes checkout is not replaced.
        assert!(
            !session_less(&install.repo, unscoped_query.clone()).contains("launch-scope"),
            "a bridge in the other repository stays in that repository"
        );
        assert!(
            !session_less(&notes, scoped_query(PROJECT)).contains("launch-scope"),
            "an explicit scope wins over the launch directory"
        );
        // #2005: the session-aware bridge's session has no pointer yet (as
        // after `/resume`), so the server falls back to its launch scope...
        let aware = |call: Value| {
            let (_, reply) = bridge_exchange_in(
                &install,
                &notes,
                BridgeMode::SessionAware,
                &base,
                Some(call),
                |_| {},
            );
            tool_text(&reply.expect("tools/call reply"))
        };
        assert!(
            aware(unscoped_query.clone()).contains("launch-scope"),
            "a session-aware call whose pointer misses answers from the launch repository"
        );
        // ...but once the hooks publish that session's project, the pointer
        // routes it and the launch scope is not used.
        install.hook_prompt(&base, "authdef-bridge-session");
        assert_eq!(settled_sessions(&client, &base, &token, 2).await, 2);
        assert!(
            !aware(unscoped_query).contains("launch-scope"),
            "the session's own pointer wins over the launch directory"
        );
    }

    /// A bridge with no token against an auth-on server exits at once with
    /// the refusal, before it serves stdio, so the client sees the server
    /// exit rather than a session that never answers.
    #[tokio::test]
    async fn a_bridge_without_the_token_is_refused_cleanly_not_hung() {
        let install = Install::new();
        install.run_ok(&["init"], None);
        let client = reqwest::Client::new();
        let (_server, base) = install.serve(&client, "serve-refused.log").await;

        let stranger = Install::new(); // no data dir token, no config
        let mut child = stranger
            .cmd()
            .args(["mcp-bridge", "--no-session-id", "--server-url", &base])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                panic!("the bridge hung on a refused connection");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
        assert!(!status.success());
        assert!(stderr.contains("sent without a bearer token"), "{stderr}");
        assert!(
            stderr.contains("401") || stderr.to_ascii_lowercase().contains("unauthorized"),
            "the refusal is surfaced: {stderr}"
        );
    }

    /// The control, and the opt-in. An install that predates the default
    /// (a database, no token file) keeps the loopback no-auth behaviour and
    /// is told how to change it; `auth enable` run twice then turns the token
    /// on for the next server start, the hooks keep delivering, Claude Code's
    /// entry becomes the bridge, and no harness config holds the token.
    #[tokio::test]
    async fn an_existing_install_is_unchanged_until_auth_enable() {
        let install = Install::new();
        std::fs::create_dir_all(install.data_dir.join("db")).unwrap();
        std::fs::write(install.data_dir.join("db").join("memory.sqlite"), b"").unwrap();
        install.run_ok(&["init"], None);
        assert_eq!(
            install.root_token(),
            None,
            "an existing install gets no token"
        );

        let client = reqwest::Client::new();
        let (server, base) = install.serve(&client, "serve-open.log").await;
        for (route, status) in route_statuses(&client, &base, None).await {
            assert!(
                status.is_success(),
                "pre-default {route} stays open: {status}"
            );
        }
        assert_eq!(healthz(&client, &base).await, StatusCode::OK);
        let status = install.run_ok(&["status"], Some(&base));
        assert!(
            status.contains("auth:         none (loopback) — fix: `ai-memory auth enable`"),
            "{status}"
        );

        std::fs::write(
            install.home.join(".claude.json"),
            json!({"mcpServers": {"ai-memory": {"type": "http", "url": format!("{base}/mcp")}}})
                .to_string(),
        )
        .unwrap();
        let first = install.run_ok(&["auth", "enable"], Some(&base));
        assert!(first.contains("generated"), "{first}");
        let token = install.root_token().expect("auth enable minted a token");
        let config_after_first =
            std::fs::read_to_string(install.data_dir.join("config.toml")).unwrap();
        let claude_after_first =
            std::fs::read_to_string(install.home.join(".claude.json")).unwrap();
        let second = install.run_ok(&["auth", "enable"], Some(&base));
        assert!(second.contains("kept"), "{second}");
        assert_eq!(install.root_token().as_deref(), Some(token.as_str()));
        assert_eq!(
            std::fs::read_to_string(install.data_dir.join("config.toml")).unwrap(),
            config_after_first
        );
        assert_eq!(
            std::fs::read_to_string(install.home.join(".claude.json")).unwrap(),
            claude_after_first
        );
        let claude: Value = serde_json::from_str(&claude_after_first).unwrap();
        assert_eq!(claude["mcpServers"]["ai-memory"]["type"], "stdio");
        assert!(claude["mcpServers"]["ai-memory"].get("env").is_none());

        // The running server read its config at start; the restart applies it.
        drop(server);
        let (server, base) = install.serve(&client, "serve-closed.log").await;
        for (route, status) in route_statuses(&client, &base, None).await {
            assert_eq!(
                status,
                StatusCode::UNAUTHORIZED,
                "after auth enable {route}"
            );
        }
        assert_eq!(healthz(&client, &base).await, StatusCode::OK);
        install.hook_prompt(&base, "authdef-enabled-session");
        assert_eq!(settled_sessions(&client, &base, &token, 1).await, 1);
        for output in [&first, &second] {
            assert!(!output.contains(&token), "token printed: {output}");
        }
        install.assert_token_confined(&token);
        drop(server);
    }

    /// The packaged layout (README quick-start, systemd unit): config outside
    /// the data dir, passed to `init` and `serve` but not to the client
    /// commands an operator types next. Those still find the token, and
    /// `install-mcp` wires Claude Code through the bridge.
    #[tokio::test]
    async fn a_packaged_install_with_an_external_config_wires_its_clients() {
        let install = Install::new();
        let config = install
            .home
            .join(".config")
            .join("ai-memory")
            .join("config.toml");
        let config = config.to_str().unwrap();
        install.run_ok(&["--config", config, "init"], None);
        let token = install.root_token().expect("init minted a root bearer");
        assert!(!install.data_dir.join("config.toml").exists());

        let client = reqwest::Client::new();
        let (_server, base) = install
            .serve_with(&client, "serve-packaged.log", &["--config", config])
            .await;
        for (route, status) in route_statuses(&client, &base, None).await {
            assert_eq!(status, StatusCode::UNAUTHORIZED, "unauthenticated {route}");
        }

        let status = install.run_ok(&["status"], Some(&base));
        assert!(status.contains("auth:         bearer token"), "{status}");
        install.run_ok(
            &["install-mcp", "--client", "claude-code", "--apply"],
            Some(&base),
        );
        let claude: Value = serde_json::from_str(
            &std::fs::read_to_string(install.home.join(".claude.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(claude["mcpServers"]["ai-memory"]["args"][0], "mcp-bridge");
        let tools = bridge_tools_list(&install, &base, |_| {});
        assert!(
            tools["result"]["tools"]
                .as_array()
                .is_some_and(|t| !t.is_empty()),
            "{tools}"
        );
        install.hook_prompt(&base, "authdef-packaged-session");
        assert_eq!(settled_sessions(&client, &base, &token, 1).await, 1);
        install.assert_token_confined(&token);
    }

    /// Uninstalling the hooks removes the hook credential files and every
    /// profile token, but keeps the root bearer: it is the server's
    /// credential. And a server whose configured token file is gone refuses to
    /// start instead of serving open.
    #[test]
    fn uninstall_keeps_the_root_bearer_and_a_missing_one_stops_the_server() {
        let install = Install::new();
        install.run_ok(&["init"], None);
        let token = install.root_token().unwrap();
        let hooks_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../hooks");
        install.run_ok(
            &[
                "install-hooks",
                "--agent",
                "claude-code",
                "--apply",
                "--hooks-dir",
                hooks_dir.to_str().unwrap(),
            ],
            None,
        );
        install.run_ok(&["uninstall", "--apply", "--only", "hooks", "--yes"], None);
        assert_eq!(install.root_token().as_deref(), Some(token.as_str()));
        assert!(!install.data_dir.join("auth-token").exists());
        assert!(!install.data_dir.join("auth-header").exists());

        std::fs::remove_file(install.data_dir.join("auth-tokens").join("default")).unwrap();
        let mut child = install
            .cmd()
            .args([
                "serve",
                "--transport",
                "http",
                "--bind",
                "127.0.0.1:0",
                "--no-watcher",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("serve kept running without its configured bearer token");
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
        assert!(!status.success());
        assert!(stderr.contains("bearer_token_file"), "{stderr}");

        // `auth enable` repairs it.
        install.run_ok(&["auth", "enable", "--no-mcp"], None);
        assert!(install.root_token().is_some());
    }

    /// The Docker wrapper's host-client handoff: a native client pointed at a
    /// copied token file through `AI_MEMORY_AUTH__BEARER_TOKEN_FILE`, with a
    /// host data dir of its own, authenticates and still treats the token as
    /// file-sourced (it is never rendered into a harness config).
    #[tokio::test]
    async fn a_host_client_given_the_token_file_by_env_authenticates() {
        let install = Install::new();
        install.run_ok(&["init"], None);
        let token = install.root_token().unwrap();
        let client = reqwest::Client::new();
        let (_server, base) = install.serve(&client, "serve-host.log").await;

        let host = Install::new();
        let copy = host.root.join("server-token");
        std::fs::write(&copy, format!("{token}\n")).unwrap();
        let mut cmd = host.cmd();
        let out = cmd
            .env("AI_MEMORY_SERVER_URL", &base)
            .env("AI_MEMORY_AUTH__BEARER_TOKEN_FILE", &copy)
            .args(["status"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "host status: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = host
            .cmd()
            .env("AI_MEMORY_SERVER_URL", &base)
            .env("AI_MEMORY_AUTH__BEARER_TOKEN_FILE", &copy)
            .args(["install-mcp", "--client", "claude-code", "--apply"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let leaks = Install::files_containing(&host.home, &token, &[]);
        assert!(leaks.is_empty(), "token in host harness files: {leaks:?}");

        // Control: without the variable the host client has no token.
        let out = host
            .cmd()
            .env("AI_MEMORY_SERVER_URL", &base)
            .args(["status"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "the host client must need the token");
    }
}
