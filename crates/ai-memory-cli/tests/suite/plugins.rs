//! The marketplace plugins (#1166): the committed `plugins/` tree and
//! catalogs against the renderer, and the plugin front door re-proved through
//! the shipped launcher and the real binary.
//!
//! A plugin is a fourth entry point beside the shell hooks, the native client
//! and the generated TypeScript integrations. It stays safe only while the
//! launcher adds no logic of its own, so every capture, scope and
//! de-duplication guarantee is asserted here by piping recorded Claude Code
//! payloads through `plugins/claude-code/scripts/ai-memory-hook` into the
//! built binary and reading the spool it writes. No server is contacted: the
//! server URL is a dead endpoint, so events stay in the spool for inspection.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::e2e_support::hermetic;

const BIN: &str = env!("CARGO_BIN_EXE_ai-memory");
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("repository root")
}

/// Every file below `dir`, as `/`-separated paths relative to `base`.
fn files_under(base: &Path, dir: &Path, out: &mut BTreeSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files_under(base, &path, out);
        } else {
            let relative = path.strip_prefix(base).expect("under base");
            out.insert(
                relative
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/"),
            );
        }
    }
}

/// The generated paths: `plugins/` plus the two catalog directories.
fn generated_files(root: &Path) -> BTreeSet<String> {
    let mut files = BTreeSet::new();
    for dir in ["plugins", ".claude-plugin", ".agents/plugins"] {
        files_under(root, &root.join(dir), &mut files);
    }
    files
}

/// The committed plugins are exactly what `render-plugins` produces: same
/// file set (no stale or hand-added file), same bytes, launcher executable.
/// This is what keeps plugin installs and `install-hooks`/`install-skills`
/// installs from drifting, and what forces `bin/release` to re-render after a
/// version bump.
#[test]
fn committed_plugins_match_the_renderer() {
    let out = tempfile::tempdir().unwrap();
    let status = hermetic(BIN)
        .args(["render-plugins", "--out"])
        .arg(out.path())
        .output()
        .expect("render-plugins");
    assert!(status.status.success(), "{status:?}");

    let root = repo_root();
    let rendered = generated_files(out.path());
    let committed = generated_files(&root);
    assert!(!rendered.is_empty());
    assert_eq!(
        committed, rendered,
        "the committed plugin tree differs from `ai-memory render-plugins`; run \
         `cargo run --bin ai-memory -- render-plugins --out .` and commit the result"
    );
    for path in &rendered {
        let want = std::fs::read(out.path().join(path)).unwrap();
        let have = std::fs::read(root.join(path)).unwrap();
        assert!(
            want == have,
            "{path} differs from the renderer; re-run `ai-memory render-plugins --out .`"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        for dir in ["claude-code", "codex"] {
            let launcher = root
                .join("plugins")
                .join(dir)
                .join("scripts/ai-memory-hook");
            let mode = std::fs::metadata(&launcher).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o111,
                0o111,
                "{} must be executable",
                launcher.display()
            );
        }
    }
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Every manifest version is the workspace version and every catalog entry
/// is pinned to its tag, so a marketplace add installs released content only.
#[test]
fn plugin_versions_and_catalog_refs_match_the_workspace() {
    let root = repo_root();
    for manifest in [
        "plugins/claude-code/.claude-plugin/plugin.json",
        "plugins/codex/.codex-plugin/plugin.json",
    ] {
        assert_eq!(
            read_json(&root.join(manifest))["version"],
            VERSION,
            "{manifest}"
        );
    }
    for catalog in [
        ".claude-plugin/marketplace.json",
        ".agents/plugins/marketplace.json",
    ] {
        let json = read_json(&root.join(catalog));
        assert_eq!(json["name"], "ai-memory", "{catalog}");
        for plugin in json["plugins"].as_array().unwrap() {
            assert_eq!(plugin["name"], "ai-memory", "{catalog}");
            assert_eq!(plugin["source"]["ref"], format!("v{VERSION}"), "{catalog}");
            assert_eq!(
                plugin["source"]["url"], "https://github.com/akitaonrails/ai-memory.git",
                "{catalog}"
            );
        }
    }
}

/// No plugin file carries a credential: nothing bearer-shaped, no ai-memory
/// user token, no provider key, no long hex secret.
#[test]
fn plugin_tree_carries_no_credentials() {
    let root = repo_root();
    for path in generated_files(&root) {
        let text = std::fs::read_to_string(root.join(&path)).unwrap();
        for needle in ["Bearer ", "aim_", "sk-ant-", "ghp_", "AI_MEMORY_AUTH_TOKEN"] {
            assert!(!text.contains(needle), "{path} contains {needle:?}");
        }
        let longest_hex_run = text
            .split(|c: char| !c.is_ascii_hexdigit())
            .map(str::len)
            .max()
            .unwrap_or(0);
        assert!(
            longest_hex_run < 32,
            "{path} has a {longest_hex_run}-digit hex run"
        );
    }
}

#[cfg(unix)]
mod launcher {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::{Output, Stdio};

    use serde_json::{Value, json};

    use super::{BIN, hermetic, repo_root};

    /// A sandboxed `$HOME`, Claude config dir, data dir and checkout.
    struct Sandbox {
        _tmp: tempfile::TempDir,
        home: PathBuf,
        claude: PathBuf,
        data: PathBuf,
        repo: PathBuf,
        server: String,
    }

    impl Sandbox {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let home = tmp.path().canonicalize().unwrap();
            let repo = home.join("work").join("repo");
            std::fs::create_dir_all(&repo).unwrap();
            let claude = home.join(".claude");
            std::fs::create_dir_all(&claude).unwrap();
            Self {
                data: home.join("data"),
                server: ai_memory_test_support::dead_http_endpoint(),
                _tmp: tmp,
                home,
                claude,
                repo,
            }
        }

        fn marker(&self, body: &str) {
            std::fs::write(self.repo.join(".ai-memory.toml"), body).unwrap();
        }

        /// Run the committed Claude Code launcher the way Claude Code does:
        /// `CLAUDE_PLUGIN_ROOT` set, options exported only when set, a PATH
        /// with no ai-memory on it.
        fn launch(&self, event: &str, payload: &Value, env: &[(&str, &str)]) -> Output {
            let plugin = repo_root().join("plugins").join("claude-code");
            let mut cmd = hermetic("sh");
            cmd.arg(plugin.join("scripts/ai-memory-hook"))
                .args(["claude-code", event])
                .current_dir(&self.repo)
                .env("HOME", &self.home)
                .env("PATH", "/usr/bin:/bin")
                .env("CLAUDE_CONFIG_DIR", &self.claude)
                .env("CLAUDE_PLUGIN_ROOT", &plugin)
                .env("CLAUDE_PLUGIN_OPTION_AI_MEMORY_BIN", BIN)
                .env("CLAUDE_PLUGIN_OPTION_DATA_DIR", &self.data)
                .env("CLAUDE_PLUGIN_OPTION_SERVER_URL", &self.server)
                .env_remove("CLAUDE_PROJECT_DIR")
                .env_remove("PLUGIN_ROOT")
                .env_remove("DROID_PLUGIN_ROOT")
                .env_remove("MUSE_PLUGIN_ID")
                .env_remove("MUSE_SESSION_ID")
                .env_remove("XDG_CONFIG_HOME")
                .env("RUST_LOG", "off")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            for (key, value) in env {
                if value.is_empty() {
                    cmd.env_remove(key);
                } else {
                    cmd.env(key, value);
                }
            }
            let mut child = cmd.spawn().expect("spawn launcher");
            child
                .stdin
                .take()
                .unwrap()
                .write_all(payload.to_string().as_bytes())
                .unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(output.status.success(), "{output:?}");
            output
        }

        fn prompt(&self) -> Value {
            json!({"session_id": "plugin-s", "cwd": self.repo, "hook_event_name": "UserPromptSubmit", "prompt": "PLUGIN_PROMPT_SENTINEL"})
        }

        fn read_tool(&self, file: &str) -> Value {
            json!({
                "session_id": "plugin-s", "cwd": self.repo, "hook_event_name": "PostToolUse",
                "tool_name": "Read",
                "tool_input": {"file_path": self.repo.join(file)},
                "tool_response": {"content": "FILE_CONTENT_SENTINEL"},
            })
        }

        fn spooled(&self) -> Vec<Value> {
            let Ok(dir) = std::fs::read_dir(self.data.join("hook-spool")) else {
                return Vec::new();
            };
            dir.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
                .map(|e| serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap())
                .collect()
        }

        fn settings_hook(&self, path: &Path, event: &str, wire: &str) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let command = format!(
                "{BIN} hook --event {wire} --agent claude-code --server-url {}",
                self.server
            );
            std::fs::write(
                path,
                json!({"hooks": {event: [{"matcher": "", "hooks": [{"type": "command", "command": command}]}]}})
                    .to_string(),
            )
            .unwrap();
        }
    }

    fn stdout(output: &Output) -> String {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    /// Control for everything below: a plugin event in an ordinary checkout
    /// reaches the spool, attributed to claude-code, with its prompt.
    #[test]
    fn a_plugin_event_reaches_the_native_hook() {
        let sb = Sandbox::new();
        let out = sb.launch("user-prompt-submit", &sb.prompt(), &[]);
        assert_eq!(stdout(&out), "{}");
        let spooled = sb.spooled();
        assert_eq!(spooled.len(), 1, "{spooled:?}");
        let url = spooled[0]["url"].as_str().unwrap();
        assert!(url.starts_with(&format!("{}/hook?", sb.server)), "{url}");
        assert!(url.contains("event=user-prompt-submit"), "{url}");
        assert!(url.contains("agent=claude-code"), "{url}");
        assert!(
            spooled[0]["body"]
                .as_str()
                .unwrap()
                .contains("PLUGIN_PROMPT_SENTINEL")
        );
    }

    /// Front door 5: no binary means nothing is sent, the agent is never
    /// blocked, guidance appears once (session-start only), stdin is drained.
    #[test]
    fn without_a_binary_nothing_is_sent_and_the_agent_is_not_blocked() {
        let sb = Sandbox::new();
        let missing = sb.home.join("no-such-ai-memory");
        let missing = missing.to_string_lossy();
        for (event, guidance) in [
            ("session-start", true),
            ("user-prompt-submit", false),
            ("post-tool-use", false),
        ] {
            let started = std::time::Instant::now();
            let out = sb.launch(
                event,
                &sb.prompt(),
                &[("CLAUDE_PLUGIN_OPTION_AI_MEMORY_BIN", &missing)],
            );
            assert!(started.elapsed() < std::time::Duration::from_secs(2));
            let reply: Value = serde_json::from_str(&stdout(&out)).expect("JSON reply");
            if guidance {
                assert!(
                    reply["systemMessage"]
                        .as_str()
                        .unwrap()
                        .contains("binary was not found")
                );
            } else {
                assert_eq!(reply, json!({}), "{event}");
            }
        }
        assert!(sb.spooled().is_empty());
        assert!(!sb.data.exists(), "nothing may touch the data dir");
    }

    /// Front door 1 (adversarial): an `ignore_paths` match never reaches the
    /// spool through the plugin. Control: a public file in the same checkout
    /// is captured.
    #[test]
    fn ignore_paths_hold_through_the_plugin() {
        let sb = Sandbox::new();
        sb.marker("[capture]\nignore_paths = [\"secret/**\"]\n");
        sb.launch("post-tool-use", &sb.read_tool("secret/token.txt"), &[]);
        assert!(
            sb.spooled().is_empty(),
            "an ignored path reached the spool: {:?}",
            sb.spooled()
        );
        sb.launch("post-tool-use", &sb.read_tool("README.md"), &[]);
        assert_eq!(sb.spooled().len(), 1);
    }

    /// Front door 1 (adversarial): a Bash call whose command names no path
    /// but which rewrote an ignored file, as Claude Code 2.1.269+ reports in
    /// `tool_response.bashEditDiff`, never reaches the spool with that file's
    /// diff. Control: the same call rewriting a public file is captured.
    #[test]
    fn ignore_paths_hold_for_a_file_a_bash_call_changed() {
        let sb = Sandbox::new();
        sb.marker("[capture]\nignore_paths = [\"secret/**\"]\n");
        let bash = |file: &str| {
            let path = sb.repo.join(file);
            json!({
                "session_id": "plugin-s", "cwd": sb.repo, "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "make gen"},
                "tool_response": {
                    "stdout": "generated\n", "stderr": "", "interrupted": false,
                    "bashEditDiff": {
                        "changedFiles": [path],
                        "files": [{"filePath": path, "hunks": [{
                            "oldStart": 1, "oldLines": 1, "newStart": 1, "newLines": 1,
                            "lines": ["-OLD_LINE_SENTINEL", "+NEW_LINE_SENTINEL"],
                        }]}],
                        "moreFiles": 0,
                    },
                },
            })
        };
        sb.launch("post-tool-use", &bash("secret/token.txt"), &[]);
        assert!(
            sb.spooled().is_empty(),
            "an ignored file's diff reached the spool: {:?}",
            sb.spooled()
        );
        sb.launch("post-tool-use", &bash("src/gen.rs"), &[]);
        assert_eq!(sb.spooled().len(), 1);
    }

    /// Front door 2 (adversarial): under allowlist capture mode a checkout
    /// without a marker emits nothing through the plugin, prompts included.
    /// Control: the same event in a marked checkout is captured.
    #[test]
    fn allowlist_mode_holds_through_the_plugin() {
        let sb = Sandbox::new();
        std::fs::create_dir_all(&sb.data).unwrap();
        std::fs::write(sb.data.join("capture-mode"), "allowlist\n").unwrap();
        sb.launch("user-prompt-submit", &sb.prompt(), &[]);
        sb.launch("post-tool-use", &sb.read_tool("README.md"), &[]);
        assert!(sb.spooled().is_empty(), "{:?}", sb.spooled());

        sb.marker("project = \"opted-in\"\n");
        sb.launch("user-prompt-submit", &sb.prompt(), &[]);
        assert_eq!(sb.spooled().len(), 1);
    }

    /// Front door 3: marker workspace/project route a plugin event exactly as
    /// a settings-launched one.
    #[test]
    fn marker_scope_routes_plugin_events() {
        let sb = Sandbox::new();
        sb.marker("workspace = \"plugin-ws\"\nproject = \"plugin-proj\"\n");
        sb.launch("user-prompt-submit", &sb.prompt(), &[]);
        let spooled = sb.spooled();
        let url = spooled[0]["url"].as_str().unwrap();
        assert!(url.contains("workspace=plugin-ws"), "{url}");
        assert!(url.contains("project=plugin-proj"), "{url}");
    }

    /// Front door 3: a `servers.toml` profile routes a plugin event to its
    /// server with its own token, never the plugin's URL.
    #[test]
    fn server_profiles_route_plugin_events() {
        let sb = Sandbox::new();
        let root = sb.repo.to_string_lossy().into_owned();
        let mut add = hermetic(BIN);
        add.env("HOME", &sb.home)
            .env("AI_MEMORY_HOME", &sb.home)
            .env("RUST_LOG", "off")
            .arg("--data-dir")
            .arg(&sb.data)
            .args([
                "server",
                "add",
                "team-p",
                "--url",
                "https://profile.example",
                "--root",
                &root,
                "--auth-token-stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = add.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"PROFILE-TOKEN-P\n")
            .unwrap();
        let added = child.wait_with_output().unwrap();
        assert!(added.status.success(), "{added:?}");

        sb.marker("workspace = \"team-p\"\nserver = \"team-p\"\n");
        sb.launch("user-prompt-submit", &sb.prompt(), &[]);
        let spooled = sb.spooled();
        assert_eq!(spooled.len(), 1);
        let url = spooled[0]["url"].as_str().unwrap();
        assert!(url.starts_with("https://profile.example/hook?"), "{url}");
        assert_eq!(spooled[0]["profile"], "team-p");
        assert_eq!(spooled[0]["token"], "PROFILE-TOKEN-P");
    }

    /// Front door 4 (adversarial): with a settings-file ai-memory hook for the
    /// same event — user settings or the checkout's settings.local.json — the
    /// plugin copy is dropped, so the event is captured once (by the settings
    /// hook). Controls: an event the settings do not wire, and no settings.
    #[test]
    fn a_settings_hook_for_the_same_event_drops_the_plugin_copy() {
        let sb = Sandbox::new();
        sb.launch("user-prompt-submit", &sb.prompt(), &[]);
        assert_eq!(sb.spooled().len(), 1, "control: no settings hook");

        sb.settings_hook(
            &sb.claude.join("settings.json"),
            "UserPromptSubmit",
            "user-prompt-submit",
        );
        let out = sb.launch("user-prompt-submit", &sb.prompt(), &[]);
        assert_eq!(stdout(&out), "{}");
        assert_eq!(sb.spooled().len(), 1, "user settings: plugin copy dropped");

        sb.launch("post-tool-use", &sb.read_tool("README.md"), &[]);
        assert_eq!(
            sb.spooled().len(),
            2,
            "an unwired event stays with the plugin"
        );

        std::fs::remove_file(sb.claude.join("settings.json")).unwrap();
        sb.settings_hook(
            &sb.repo.join(".claude").join("settings.local.json"),
            "PostToolUse",
            "post-tool-use",
        );
        sb.launch("post-tool-use", &sb.read_tool("README.md"), &[]);
        assert_eq!(
            sb.spooled().len(),
            2,
            "project settings: plugin copy dropped"
        );
        let dir = sb.repo.to_string_lossy().into_owned();
        sb.launch(
            "post-tool-use",
            &sb.read_tool("README.md"),
            &[("CLAUDE_PROJECT_DIR", &dir)],
        );
        assert_eq!(
            sb.spooled().len(),
            2,
            "CLAUDE_PROJECT_DIR names the checkout"
        );
    }

    /// Front door 4 for Droid (adversarial): Droid runs the Claude Code
    /// plugin with `DROID_PLUGIN_ROOT`, and the launcher re-attributes it.
    /// With the `install-hooks --agent droid` hook for the same event in
    /// `~/.factory/hooks.json` the plugin copy is dropped. Controls: no Droid
    /// hooks keeps it, and so does a Claude settings hook, which Droid never
    /// runs. A folderless Droid session is not captured either.
    #[test]
    fn a_droid_hook_for_the_same_event_drops_the_droid_plugin_copy() {
        let sb = Sandbox::new();
        let root = sb.home.to_string_lossy().into_owned();
        let droid = [
            ("CLAUDE_PLUGIN_ROOT", ""),
            ("DROID_PLUGIN_ROOT", root.as_str()),
        ];

        sb.settings_hook(
            &sb.claude.join("settings.json"),
            "UserPromptSubmit",
            "user-prompt-submit",
        );
        sb.launch("user-prompt-submit", &sb.prompt(), &droid);
        assert_eq!(sb.spooled().len(), 1, "control: no Droid hook");
        assert!(
            sb.spooled()[0]["url"]
                .as_str()
                .unwrap()
                .contains("agent=droid")
        );

        let factory = sb.home.join(".factory");
        std::fs::create_dir_all(&factory).unwrap();
        let command = format!(
            "{BIN} hook --event user-prompt-submit --agent droid --server-url {}",
            sb.server
        );
        std::fs::write(
            factory.join("hooks.json"),
            json!({"UserPromptSubmit": [{"matcher": "", "hooks": [{"type": "command", "command": command}]}]})
                .to_string(),
        )
        .unwrap();
        sb.launch("user-prompt-submit", &sb.prompt(), &droid);
        assert_eq!(
            sb.spooled().len(),
            1,
            "Droid hooks.json: plugin copy dropped"
        );

        sb.launch("post-tool-use", &sb.read_tool("README.md"), &droid);
        assert_eq!(
            sb.spooled().len(),
            2,
            "an unwired event stays with the plugin"
        );

        sb.launch(
            "user-prompt-submit",
            &json!({"session_id": "s", "prompt": "no cwd"}),
            &droid,
        );
        assert_eq!(sb.spooled().len(), 2, "folderless Droid session");
    }

    /// Front door 4 for Muse Code (adversarial): Muse loads the Claude Code
    /// plugin with `CLAUDE_PLUGIN_ROOT` set and adds `MUSE_PLUGIN_ID` and
    /// `MUSE_SESSION_ID`, and the launcher re-attributes it to `muse`. With
    /// the `install-hooks --agent muse` hook for the same event under the
    /// `hooks` key of `~/.config/muse/settings.json` the plugin copy is
    /// dropped. Controls: no Muse hooks keeps it, and so does a Claude
    /// settings hook, which Muse never runs; an unwired event stays with the
    /// plugin. The plugin's subagent events and a folderless session are
    /// never captured.
    #[test]
    fn a_muse_hook_for_the_same_event_drops_the_muse_plugin_copy() {
        let sb = Sandbox::new();
        let muse = [
            ("MUSE_PLUGIN_ID", "ai-memory"),
            ("MUSE_SESSION_ID", "01a13669-dc61-7a72-b5ec-ad2f300ffd26"),
        ];

        sb.settings_hook(
            &sb.claude.join("settings.json"),
            "UserPromptSubmit",
            "user-prompt-submit",
        );
        sb.launch("user-prompt-submit", &sb.prompt(), &muse);
        assert_eq!(sb.spooled().len(), 1, "control: no Muse hook");
        assert!(
            sb.spooled()[0]["url"]
                .as_str()
                .unwrap()
                .contains("agent=muse")
        );

        let settings = sb.home.join(".config").join("muse");
        std::fs::create_dir_all(&settings).unwrap();
        let command = format!(
            "{BIN} hook --event user-prompt-submit --agent muse --server-url {}",
            sb.server
        );
        std::fs::write(
            settings.join("settings.json"),
            json!({
                "schema_version": 1,
                "hooks": {"UserPromptSubmit": [{"matcher": "", "hooks": [{"type": "command", "command": command, "timeout": 30}]}]}
            })
            .to_string(),
        )
        .unwrap();
        sb.launch("user-prompt-submit", &sb.prompt(), &muse);
        assert_eq!(
            sb.spooled().len(),
            1,
            "Muse settings.json: plugin copy dropped"
        );

        sb.launch("post-tool-use", &sb.read_tool("README.md"), &muse);
        assert_eq!(
            sb.spooled().len(),
            2,
            "an unwired event stays with the plugin"
        );

        sb.launch("subagent-start", &sb.prompt(), &muse);
        assert_eq!(sb.spooled().len(), 2, "Muse subagent events are dropped");

        sb.launch(
            "user-prompt-submit",
            &json!({"session_id": "s", "prompt": "no cwd"}),
            &muse,
        );
        assert_eq!(sb.spooled().len(), 2, "folderless Muse session");
    }

    /// The committed hooks.json command still works when the launcher lost
    /// its exec bit (a desktop zip upload, a Windows checkout): Claude Code
    /// runs the command line through a shell, and it starts the launcher
    /// with `sh` instead of executing it.
    #[test]
    fn the_hook_command_runs_a_launcher_without_the_exec_bit() {
        use std::os::unix::fs::PermissionsExt as _;

        let sb = Sandbox::new();
        let source = repo_root().join("plugins").join("claude-code");
        let plugin = sb.home.join("unzipped-plugin");
        std::fs::create_dir_all(plugin.join("scripts")).unwrap();
        let launcher = plugin.join("scripts/ai-memory-hook");
        std::fs::copy(source.join("scripts/ai-memory-hook"), &launcher).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o644)).unwrap();
        let hooks: Value = serde_json::from_str(
            &std::fs::read_to_string(source.join("hooks/hooks.json")).unwrap(),
        )
        .unwrap();
        let command = hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();

        let mut cmd = hermetic("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&sb.repo)
            .env("HOME", &sb.home)
            .env("PATH", "/usr/bin:/bin")
            .env("CLAUDE_CONFIG_DIR", &sb.claude)
            .env("CLAUDE_PLUGIN_ROOT", &plugin)
            .env("CLAUDE_PLUGIN_OPTION_AI_MEMORY_BIN", BIN)
            .env("CLAUDE_PLUGIN_OPTION_DATA_DIR", &sb.data)
            .env("CLAUDE_PLUGIN_OPTION_SERVER_URL", &sb.server)
            .env_remove("CLAUDE_PROJECT_DIR")
            .env_remove("PLUGIN_ROOT")
            .env_remove("DROID_PLUGIN_ROOT")
            .env_remove("MUSE_PLUGIN_ID")
            .env_remove("MUSE_SESSION_ID")
            .env_remove("XDG_CONFIG_HOME")
            .env("RUST_LOG", "off")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(sb.prompt().to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(sb.spooled().len(), 1, "{output:?}");
    }

    /// A session with no project folder is not captured through the plugin.
    #[test]
    fn folderless_sessions_are_not_captured_through_the_plugin() {
        let sb = Sandbox::new();
        let scratch = sb
            .home
            .join(".config/Claude/scratch-workspaces/acct/org/scratch-2026-10-13-ab12");
        std::fs::create_dir_all(&scratch).unwrap();
        sb.launch(
            "user-prompt-submit",
            &json!({"session_id": "s", "prompt": "no cwd"}),
            &[],
        );
        sb.launch(
            "user-prompt-submit",
            &json!({"session_id": "s", "cwd": scratch, "prompt": "scratch"}),
            &[],
        );
        assert!(sb.spooled().is_empty(), "{:?}", sb.spooled());
    }

    /// An unset server option reaches the binary as "" and the install's
    /// configured server applies.
    #[test]
    fn an_unset_server_option_uses_the_configured_server() {
        let sb = Sandbox::new();
        let configured = ai_memory_test_support::dead_http_endpoint();
        sb.launch(
            "user-prompt-submit",
            &sb.prompt(),
            &[
                ("CLAUDE_PLUGIN_OPTION_SERVER_URL", ""),
                ("AI_MEMORY_SERVER_URL", &configured),
            ],
        );
        let url = sb.spooled()[0]["url"].as_str().unwrap().to_owned();
        assert!(url.starts_with(&format!("{configured}/hook?")), "{url}");
    }

    /// Other harnesses that read the Claude plugin through a fallback get
    /// their own agent, or nothing when ai-memory has none for them.
    #[test]
    fn foreign_hosts_are_attributed_or_ignored() {
        let sb = Sandbox::new();
        let root = sb.home.to_string_lossy().into_owned();
        sb.launch(
            "user-prompt-submit",
            &sb.prompt(),
            &[("CLAUDE_PLUGIN_ROOT", ""), ("DROID_PLUGIN_ROOT", &root)],
        );
        sb.launch(
            "user-prompt-submit",
            &sb.prompt(),
            &[("CLAUDE_PLUGIN_ROOT", ""), ("PLUGIN_ROOT", &root)],
        );
        sb.launch(
            "user-prompt-submit",
            &sb.prompt(),
            &[
                ("MUSE_PLUGIN_ID", "ai-memory"),
                ("MUSE_SESSION_ID", "01a13669-dc61-7a72-b5ec-ad2f300ffd26"),
            ],
        );
        let out = sb.launch(
            "user-prompt-submit",
            &sb.prompt(),
            &[("CLAUDE_PLUGIN_ROOT", "")],
        );
        assert_eq!(stdout(&out), "{}");
        let mut agents: Vec<String> = sb
            .spooled()
            .iter()
            .map(|entry| {
                let url = entry["url"].as_str().unwrap();
                url.split('&')
                    .find_map(|part| part.strip_prefix("agent="))
                    .unwrap()
                    .to_owned()
            })
            .collect();
        agents.sort();
        assert_eq!(agents, vec!["codex", "droid", "muse"]);
    }
}
