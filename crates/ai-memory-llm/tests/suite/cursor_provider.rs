//! Offline tests for the Cursor Agent CLI provider.
//!
//! The fake executable is a shell script, so these tests are Unix-only.
//! They assert the argv contract: ask mode, no `--yolo` / `--force`.

#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use ai_memory_llm::{
    CURSOR_DEFAULT_MODEL, ChatRequest, CursorAgentProvider, CursorAuth, LlmError, LlmProvider,
    ProviderAuth, with_generation_attempt_guard,
};

use super::generation_attempt_guard::RecordingGuard;

fn sh_single(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn fake_agent(dir: &Path, sink: &Path, stdout: &str) -> PathBuf {
    let path = dir.join("agent");
    let script = format!(
        "#!/bin/sh\nmode=$(stat -c %a . 2>/dev/null || stat -f %Lp .)\nprintf '%s\\n' \"$@\" > {}\nprintf '%s\\n' \"$mode\" >> {}\ncase \" $* \" in\n  *' --yolo '*|*' --force '*) echo refusing >&2; exit 2 ;;\nesac\nprintf '%s\\n' {}\n",
        sh_single(&sink.display().to_string()),
        sh_single(&sink.display().to_string()),
        sh_single(stdout),
    );
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("ai-memory-cursor-test-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The fake agents are shell scripts that need `PATH`; the workspace root is
/// the fake agent's own temp dir.
fn provider(executable: PathBuf, model: &str) -> CursorAgentProvider {
    let path = std::env::var_os("PATH").unwrap_or_default();
    provider_with_env(executable, model, &[(OsString::from("PATH"), path)])
}

fn provider_with_env(
    executable: PathBuf,
    model: &str,
    process_env: &[(OsString, OsString)],
) -> CursorAgentProvider {
    let root = executable.parent().unwrap().to_path_buf();
    CursorAgentProvider::new(
        ProviderAuth::cursor(CursorAuth::new(executable, root, process_env))
            .require_cursor_auth()
            .expect("cursor executable resolves"),
        model,
    )
}

#[tokio::test]
async fn ask_mode_returns_text_without_yolo() {
    let dir = TempDir::new();
    let sink = dir.0.join("argv");
    let executable = fake_agent(&dir.0, &sink, "OK");
    let response = provider(executable, CURSOR_DEFAULT_MODEL)
        .complete(ChatRequest::user_prompt("Reply with OK"))
        .await
        .unwrap();
    assert_eq!(response.text, "OK");
    let argv = std::fs::read_to_string(&sink).unwrap();
    assert!(argv.contains("--mode\nask\n"), "{argv}");
    assert!(argv.contains("--print\n"), "{argv}");
    assert!(!argv.contains("--yolo"), "{argv}");
    assert!(!argv.contains("--force"), "{argv}");
    assert!(argv.contains(CURSOR_DEFAULT_MODEL), "{argv}");
    assert!(argv.lines().any(|line| line == "700"), "{argv}");
}

#[tokio::test]
async fn default_model_omits_the_model_flag() {
    let dir = TempDir::new();
    let sink = dir.0.join("argv");
    let executable = fake_agent(&dir.0, &sink, "{\"answer\":\"ok\"}");
    let value = provider(executable, "default")
        .complete_structured_raw(
            ChatRequest::user_prompt("status"),
            serde_json::json!({"type": "object"}),
        )
        .await
        .unwrap();
    assert_eq!(value["answer"], "ok");
    let argv = std::fs::read_to_string(&sink).unwrap();
    assert!(!argv.contains("--model"), "{argv}");
}

#[tokio::test]
async fn fenced_json_is_parsed() {
    let dir = TempDir::new();
    let sink = dir.0.join("argv");
    let executable = fake_agent(&dir.0, &sink, "```json\n{\"answer\":1}\n```");
    let value = provider(executable, "default")
        .complete_structured_raw(
            ChatRequest::user_prompt("status"),
            serde_json::json!({"type": "object"}),
        )
        .await
        .unwrap();
    assert_eq!(value["answer"], 1);
}

/// Ask mode often wraps the answer in prose. Brackets in that prose must not
/// hide a well-formed JSON object from the parser.
#[tokio::test]
async fn json_wrapped_in_bracketed_prose_is_parsed() {
    for stdout in [
        "Here is the result for [the request]:\n{\"answer\":1}",
        "{\"answer\":1}\n(see [schema])",
    ] {
        let dir = TempDir::new();
        let sink = dir.0.join("argv");
        let executable = fake_agent(&dir.0, &sink, stdout);
        let value = provider(executable, "default")
            .complete_structured_raw(
                ChatRequest::user_prompt("status"),
                serde_json::json!({"type": "object"}),
            )
            .await
            .unwrap_or_else(|err| panic!("{stdout:?}: {err}"));
        assert_eq!(value["answer"], 1, "{stdout:?}");
    }
}

fn script_agent(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("agent");
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

/// A child that floods stderr past the pipe buffer before writing stdout
/// must not hang the call: both pipes drain together, and a stream over its
/// cap ends the call at once instead of waiting out the timeout.
#[tokio::test]
async fn a_stderr_flood_neither_hangs_nor_blocks_stdout() {
    let dir = TempDir::new();
    // 40 KiB of stderr, then the answer: within the cap, so it succeeds.
    let ok = script_agent(
        &dir.0,
        "head -c 40960 /dev/zero | tr '\\0' e >&2\nprintf 'OK\\n'",
    );
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        provider(ok, "default").complete(ChatRequest::user_prompt("hi")),
    )
    .await
    .expect("a 40 KiB stderr must not hang the call")
    .unwrap();
    assert_eq!(response.text, "OK");

    // 200 KiB of stderr while stdout stays open: over the cap, so it fails
    // fast instead of blocking on the full stderr pipe.
    let flood = script_agent(
        &dir.0,
        "head -c 204800 /dev/zero | tr '\\0' e >&2\nsleep 30\nprintf 'late\\n'",
    );
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        provider(flood, "default").complete(ChatRequest::user_prompt("hi")),
    )
    .await
    .expect("a stderr flood must not hang the call")
    .unwrap_err();
    assert!(err.to_string().contains("size cap"), "{err}");
}

/// The per-call workspace holds the prompt (captured session text), so it is
/// created private to the server's user.
#[tokio::test]
async fn the_prompt_workspace_is_private() {
    let dir = TempDir::new();
    let sink = dir.0.join("mode");
    let agent = script_agent(
        &dir.0,
        &format!(
            "ls -ld . > {}\nprintf 'OK\\n'",
            sh_single(&sink.display().to_string())
        ),
    );
    provider(agent, "default")
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .unwrap();
    let listing = std::fs::read_to_string(&sink).unwrap();
    assert!(listing.starts_with("drwx------"), "{listing}");
}

/// The child reads untrusted captured session text, so it runs with the
/// allowlisted environment only: the server's bearer and another provider's
/// key never reach it, and nothing leaks from the parent process itself
/// (cargo sets `CARGO_*` in every test process). Control: the allowlisted
/// `PATH`, `HOME` and `CURSOR_API_KEY` arrive, and the workspace sits under
/// the injected root rather than the process temp dir.
#[tokio::test]
async fn the_child_gets_only_the_allowlisted_environment() {
    let dir = TempDir::new();
    let sink = dir.0.join("env");
    let agent = script_agent(
        &dir.0,
        &format!(
            "env > {sink}\npwd >> {sink}\nprintf 'OK\\n'",
            sink = sh_single(&sink.display().to_string())
        ),
    );
    let process_env: Vec<(OsString, OsString)> = [
        ("PATH", std::env::var("PATH").unwrap_or_default()),
        ("HOME", "/home/operator".to_string()),
        ("CURSOR_API_KEY", "cursor-key-ok".to_string()),
        ("AI_MEMORY_AUTH_TOKEN", "root-bearer-canary".to_string()),
        ("OPENAI_API_KEY", "sk-openai-canary".to_string()),
    ]
    .into_iter()
    .map(|(name, value)| (OsString::from(name), OsString::from(value)))
    .collect();

    let response = provider_with_env(agent, "default", &process_env)
        .complete(ChatRequest::user_prompt("hi"))
        .await
        .unwrap();
    assert_eq!(response.text, "OK");

    let seen = std::fs::read_to_string(&sink).unwrap();
    assert!(!seen.contains("root-bearer-canary"), "{seen}");
    assert!(!seen.contains("sk-openai-canary"), "{seen}");
    assert!(
        !seen.lines().any(|line| line.starts_with("CARGO_")),
        "the parent's own environment must not be inherited: {seen}"
    );
    assert!(seen.contains("HOME=/home/operator\n"), "{seen}");
    assert!(seen.contains("CURSOR_API_KEY=cursor-key-ok\n"), "{seen}");
    let workspace = seen.lines().last().unwrap();
    let root = std::fs::canonicalize(&dir.0).unwrap();
    assert!(
        Path::new(workspace).starts_with(&dir.0) || Path::new(workspace).starts_with(&root),
        "workspace {workspace} must sit under the injected root"
    );
}

/// A relative workspace root would put the prompt under the server's cwd;
/// the provider refuses instead.
#[tokio::test]
async fn a_relative_workspace_root_is_refused() {
    let auth = CursorAuth::new("agent", "relative-root", &[]);
    let err = CursorAgentProvider::new(
        ProviderAuth::cursor(auth).require_cursor_auth().unwrap(),
        "default",
    )
    .complete(ChatRequest::user_prompt("hi"))
    .await
    .unwrap_err();
    assert!(err.to_string().contains("absolute"), "{err}");
}

/// One CLI execution is one generation send: the guard fires once per call,
/// and a refusal stops the agent from running at all.
#[tokio::test]
async fn the_guard_fires_per_execution_and_a_refusal_prevents_it() {
    let dir = TempDir::new();
    let runs = dir.0.join("runs");
    let agent = script_agent(
        &dir.0,
        &format!(
            "echo run >> {}\nprintf 'OK\\n'",
            sh_single(&runs.display().to_string())
        ),
    );
    let cursor = provider(agent, "default");
    let guard = RecordingGuard::new(0);
    let (first, second) = with_generation_attempt_guard(guard.clone(), async {
        let first = cursor.complete(ChatRequest::user_prompt("hi")).await;
        let second = cursor.complete(ChatRequest::user_prompt("hi")).await;
        (first, second)
    })
    .await;
    assert_eq!(first.unwrap().text, "OK");
    assert!(
        matches!(second, Err(LlmError::CapacityBusy(_))),
        "{second:?}"
    );
    assert_eq!(guard.flags(), vec![true, false]);
    assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 1);
}
