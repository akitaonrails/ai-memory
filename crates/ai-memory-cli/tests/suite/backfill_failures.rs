//! Real CLI exit status and output when the hook batch endpoint fails.
#![cfg(unix)]
use axum::{Json, Router, http::StatusCode, routing::post};
use serde_json::{Value, json};

#[tokio::test]
async fn import_failures_are_errors_in_human_json_and_quiet_modes() {
    for partial in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(project.path()).unwrap();
        let transcripts = home
            .path()
            .join(".claude/projects")
            .join(cwd.to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&transcripts).unwrap();
        for sid in [
            "11111111-2222-3333-4444-555555555555",
            "99999999-2222-3333-4444-555555555555",
        ] {
            crate::e2e_support::write_jsonl(
                &transcripts.join(format!("{sid}.jsonl")),
                &[
                    json!({"sessionId": sid, "cwd": cwd}),
                    json!({"type": "user", "message": {"role": "user", "content": [{"type":"text", "text":"fixture history"}]}}),
                ],
            );
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/hook/batch",
            post(move |Json(items): Json<Vec<Value>>| async move {
                if partial && items[0]["url"].as_str().unwrap().contains("99999999") {
                    (StatusCode::OK, Json(json!({"accepted": items.len()})))
                } else {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "injected batch failure"})),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for mode in ["human", "json", "quiet"] {
            let data = tempfile::tempdir().unwrap();
            let mut cmd = crate::e2e_support::hermetic(env!("CARGO_BIN_EXE_ai-memory"));
            cmd.env_remove("CODEX_HOME");
            cmd.current_dir(&cwd)
                .env("AI_MEMORY_HOME", home.path())
                .env("AI_MEMORY_DATA_DIR", data.path())
                .env("AI_MEMORY_SERVER_URL", format!("http://{addr}"))
                .args([
                    "backfill",
                    "--workspace",
                    "review",
                    "--project",
                    "fixture",
                    "--force",
                ]);
            if mode == "json" {
                cmd.arg("--json");
            }
            if mode == "quiet" {
                cmd.args(["--auto", "--quiet"]);
            }
            let output = tokio::process::Command::from(cmd).output().await.unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                !output.status.success(),
                "{mode}, partial={partial}: falsely succeeded: {stdout}"
            );
            assert!(
                stderr.contains("injected batch failure"),
                "failure detail lost: {stderr}"
            );
            if mode == "json" {
                let report: Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(report["selected"], 2);
                assert_eq!(report["imported_sessions"], usize::from(partial));
                assert_eq!(report["failed_sessions"], if partial { 1 } else { 2 });
            } else if mode == "quiet" {
                assert!(stdout.is_empty());
            } else {
                assert!(
                    stdout.contains("failed"),
                    "human summary must include failures: {stdout}"
                );
            }
        }
        server.abort();
    }
}
