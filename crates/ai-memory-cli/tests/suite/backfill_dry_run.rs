//! A planning-only backfill must not suppress the next automatic import.
use axum::{Json, Router, routing::get};
use serde_json::json;

#[tokio::test]
async fn dry_run_preserves_sentinel_for_empty_populated_and_opted_out_projects() {
    for (sessions, auto) in [(0, false), (1, false), (0, true)] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/admin/sessions/by-agent",
            get(move || async move {
                Json(json!({"by_agent": [{"agent": "claude-code", "sessions": sessions}]}))
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let state = data.path().join("backfill-state");
        // Exercise both an absent marker and a marker from an earlier attempt.
        for existing in [false, true] {
            if existing {
                std::fs::create_dir_all(&state).unwrap();
                use sha2::{Digest, Sha256};
                let canonical = std::fs::canonicalize(cwd.path()).unwrap();
                let hash = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
                std::fs::write(state.join(format!("{hash:x}")), b"keep-existing").unwrap();
            }
            let mut cmd = crate::e2e_support::hermetic(env!("CARGO_BIN_EXE_ai-memory"));
            cmd.current_dir(cwd.path())
                .env("AI_MEMORY_HOME", home.path())
                .env("AI_MEMORY_DATA_DIR", data.path())
                .env("AI_MEMORY_SERVER_URL", format!("http://{addr}"))
                .env("AI_MEMORY_BACKFILL_ON_START", "false")
                .args([
                    "backfill",
                    "--workspace",
                    "review",
                    "--project",
                    "fixture",
                    "--dry-run",
                ]);
            if auto {
                cmd.arg("--auto");
            }
            let output = tokio::process::Command::from(cmd).output().await.unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if existing {
                let paths: Vec<_> = std::fs::read_dir(&state).unwrap().collect();
                assert_eq!(paths.len(), 1);
                assert_eq!(
                    std::fs::read(paths[0].as_ref().unwrap().path()).unwrap(),
                    b"keep-existing"
                );
            } else {
                assert!(
                    !state.exists(),
                    "dry-run created backfill-state: sessions={sessions}, auto={auto}"
                );
            }
        }
        server.abort();
    }
}
