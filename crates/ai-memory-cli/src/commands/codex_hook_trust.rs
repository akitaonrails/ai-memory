//! Whether Codex will actually run ai-memory's hooks.
//!
//! Codex runs a non-managed hook only after the user trusts it, and records
//! that trust against a hash of the hook's event, matcher and command. A hook
//! that was never trusted, or whose command changed after it was (a new
//! binary path, data dir or server URL), is skipped without any message, so
//! every Codex session goes uncaptured while `doctor`'s coverage table only
//! shows the symptom. Codex itself is the authority on the hash, so this asks
//! it — `codex app-server`'s `hooks/list` — instead of re-deriving it.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// How long Codex gets to start and answer before the probe gives up.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The trust state of the ai-memory hooks in one Codex hooks file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CodexHookTrust {
    /// Event names (`sessionStart`, ...) of ai-memory hooks Codex will run.
    pub(crate) trusted: Vec<String>,
    /// Event names of ai-memory hooks Codex skips, with the reason Codex gives
    /// (`untrusted` or `modified`).
    pub(crate) skipped: Vec<(String, String)>,
}

#[derive(Deserialize)]
struct ListResponse {
    data: Vec<ListEntry>,
}

#[derive(Deserialize)]
struct ListEntry {
    hooks: Vec<HookMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookMetadata {
    event_name: String,
    trust_status: String,
    source_path: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    plugin_id: Option<String>,
}

/// Which ai-memory hooks a probe reports on.
#[derive(Debug, Clone, Copy)]
pub(crate) enum HookSource<'a> {
    /// The hooks `install-hooks` wrote to this hooks file.
    File(&'a Path),
    /// The hooks of the installed plugin with this id (#1166).
    Plugin(&'a str),
}

/// Ask `codex` (the program at `program`) for the trust state of the
/// ai-memory hooks in `hooks_file`. `None` when Codex is not installed, does
/// not answer in time, speaks a protocol this does not understand, or lists
/// no ai-memory hook in that file — the caller reports the state as unknown.
pub(crate) async fn probe(
    program: &Path,
    source: HookSource<'_>,
    cwd: &Path,
) -> Option<CodexHookTrust> {
    let result = tokio::time::timeout(PROBE_TIMEOUT, list_hooks(program, cwd)).await;
    let response = result.ok()??;
    summarize(&response, source)
}

/// Spawn, retrying briefly while the executable is busy (ETXTBSY): Codex
/// updates itself in place, and on Linux a write that just finished can still
/// hold the file open in a process another thread forked, as the OpenCode
/// version probe also allows for. The 80 ms window stays inside the probe
/// timeout.
async fn spawn_retrying_busy(
    command: &mut tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    const ATTEMPTS: u32 = 5;
    let mut attempt = 1;
    loop {
        match command.spawn() {
            Err(error)
                if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < ATTEMPTS =>
            {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            result => return result,
        }
    }
}

async fn list_hooks(program: &Path, cwd: &Path) -> Option<serde_json::Value> {
    let mut command = tokio::process::Command::new(program);
    command
        .arg("app-server")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = spawn_retrying_busy(&mut command).await.ok()?;
    let mut stdin = child.stdin.take()?;
    let stdout = child.stdout.take()?;
    let cwd = cwd.to_string_lossy();
    let requests = [
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"clientInfo": {"name": "ai-memory-doctor", "version": env!("CARGO_PKG_VERSION")}}}),
        serde_json::json!({"jsonrpc": "2.0", "method": "initialized"}),
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "hooks/list",
            "params": {"cwds": [cwd]}}),
    ];
    for request in requests {
        let mut line = request.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).await.ok()?;
    }
    stdin.flush().await.ok()?;
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if message.get("id") == Some(&serde_json::json!(2)) {
            return message.get("result").cloned();
        }
    }
    None
}

/// The trust state of the ai-memory hooks `source` names, from a
/// `hooks/list` result. A hooks-file hook is ai-memory's when its command runs
/// `ai-memory ... hook --event ...`, the form `install-hooks` writes; a plugin
/// hook is ai-memory's when Codex reports it under the plugin's id.
fn summarize(result: &serde_json::Value, source: HookSource<'_>) -> Option<CodexHookTrust> {
    let response = serde_json::from_value::<ListResponse>(result.clone()).ok()?;
    let mut trust = CodexHookTrust {
        trusted: Vec::new(),
        skipped: Vec::new(),
    };
    for hook in response.data.into_iter().flat_map(|entry| entry.hooks) {
        let ours = match source {
            HookSource::File(hooks_file) => {
                Path::new(&hook.source_path) == hooks_file
                    && hook.command.as_deref().is_some_and(|command| {
                        command.contains("ai-memory") && command.contains(" hook --event ")
                    })
            }
            HookSource::Plugin(id) => hook.plugin_id.as_deref() == Some(id),
        };
        if !ours {
            continue;
        }
        match hook.trust_status.as_str() {
            "trusted" | "managed" => trust.trusted.push(hook.event_name),
            other => trust.skipped.push((hook.event_name, other.to_owned())),
        }
    }
    (!trust.trusted.is_empty() || !trust.skipped.is_empty()).then_some(trust)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: &str, status: &str, source: &str, command: &str) -> serde_json::Value {
        serde_json::json!({
            "eventName": event, "trustStatus": status, "sourcePath": source,
            "handlerType": "command", "command": command, "currentHash": "sha256:x",
        })
    }

    #[test]
    fn summarizes_only_ai_memory_hooks_from_the_installed_file() {
        let file = "/home/me/.codex/hooks.json";
        let ours = "/home/me/.local/share/ai-memory/native-runner/ai-memory --data-dir /d hook --event session-start --agent codex";
        let result = serde_json::json!({"data": [{"cwd": "/repo", "warnings": [], "errors": [], "hooks": [
            hook("sessionStart", "trusted", file, ours),
            hook("stop", "modified", file, ours),
            hook("preToolUse", "untrusted", file, ours),
            hook("postToolUse", "untrusted", file, "/usr/bin/other-tool --check"),
            hook("userPromptSubmit", "untrusted", "/repo/.codex/hooks.json", ours),
        ]}]});
        let trust = summarize(&result, HookSource::File(Path::new(file))).unwrap();
        assert_eq!(trust.trusted, vec!["sessionStart"]);
        assert_eq!(
            trust.skipped,
            vec![
                ("stop".to_owned(), "modified".to_owned()),
                ("preToolUse".to_owned(), "untrusted".to_owned()),
            ]
        );
    }

    /// Plugin hooks run the launcher, not `ai-memory hook`, so they are
    /// recognised by the plugin id Codex reports; another plugin's hooks and
    /// the hooks-file hooks are not counted.
    #[test]
    fn summarizes_the_ai_memory_plugin_hooks_by_plugin_id() {
        let cache = "/home/me/.codex/plugins/cache/ai-memory/ai-memory/3.0.0/hooks/hooks.json";
        let launcher = "sh \"/home/me/.codex/plugins/cache/ai-memory/ai-memory/3.0.0/scripts/ai-memory-hook\" codex stop";
        let mut ours = hook("sessionStart", "trusted", cache, launcher);
        ours["pluginId"] = "ai-memory@ai-memory".into();
        let mut stale = hook("stop", "modified", cache, launcher);
        stale["pluginId"] = "ai-memory@ai-memory".into();
        let mut other = hook("stop", "untrusted", "/x/hooks.json", "other");
        other["pluginId"] = "other@market".into();
        let file = hook(
            "preToolUse",
            "untrusted",
            "/home/me/.codex/hooks.json",
            "ai-memory hook --event pre-tool-use --agent codex",
        );
        let result = serde_json::json!({"data": [{"hooks": [ours, stale, other, file]}]});
        let trust = summarize(&result, HookSource::Plugin("ai-memory@ai-memory")).unwrap();
        assert_eq!(trust.trusted, vec!["sessionStart"]);
        assert_eq!(
            trust.skipped,
            vec![("stop".to_owned(), "modified".to_owned())]
        );
    }

    #[test]
    fn no_ai_memory_hook_or_an_unknown_shape_is_unknown() {
        let file = Path::new("/home/me/.codex/hooks.json");
        let other = serde_json::json!({"data": [{"hooks": [
            hook("stop", "untrusted", "/home/me/.codex/hooks.json", "/usr/bin/other-tool"),
        ]}]});
        assert_eq!(summarize(&other, HookSource::File(file)), None);
        assert_eq!(
            summarize(
                &serde_json::json!({"unexpected": true}),
                HookSource::File(file)
            ),
            None
        );
    }

    /// The probe drives a real process over stdio: a stand-in `codex` that
    /// answers `hooks/list` proves the request sequence and the reply parse.
    #[cfg(unix)]
    #[tokio::test]
    async fn probe_reads_hooks_list_from_the_app_server() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let hooks_file = temp.path().join("hooks.json");
        let reply = serde_json::json!({"jsonrpc": "2.0", "id": 2, "result": {"data": [{"hooks": [
            hook("sessionStart", "modified", &hooks_file.to_string_lossy(),
                 "ai-memory hook --event session-start --agent codex"),
        ]}]}});
        let fake = temp.path().join("codex");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n[ \"$1\" = app-server ] || exit 2\nread -r _init\nread -r _ready\nread -r list\ncase \"$list\" in *hooks/list*) ;; *) exit 3 ;; esac\necho '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{}}}}'\necho '{reply}'\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let trust = probe(&fake, HookSource::File(&hooks_file), temp.path())
            .await
            .unwrap();
        assert!(trust.trusted.is_empty());
        assert_eq!(
            trust.skipped,
            vec![("sessionStart".to_owned(), "modified".to_owned())]
        );
        assert_eq!(
            probe(
                &temp.path().join("missing"),
                HookSource::File(&hooks_file),
                temp.path()
            )
            .await,
            None
        );
    }
}
