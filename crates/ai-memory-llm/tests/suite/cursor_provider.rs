//! Offline tests for the Cursor Agent CLI provider.
//!
//! The fake executable is a shell script, so these tests are Unix-only.
//! They assert the argv contract: ask mode, no `--yolo` / `--force`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use ai_memory_llm::{
    CURSOR_DEFAULT_MODEL, ChatRequest, CursorAgentProvider, LlmProvider, ProviderAuth,
};

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

fn provider(executable: PathBuf, model: &str) -> CursorAgentProvider {
    CursorAgentProvider::new(
        ProviderAuth::cursor(executable)
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
