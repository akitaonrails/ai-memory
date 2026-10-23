//! The shipped CLI renders provenance and accepts older server DTOs.

use axum::{Json, Router, routing::get};
use serde_json::{Value, json};

#[tokio::test]
async fn workstream_search_renders_provenance_and_accepts_older_servers() {
    let workstream = "018f0000-0000-7000-8000-000000000001";
    for provenance in [true, false] {
        let mut event = json!({"sequence": 1, "event_id": "event-1", "agent": "codex",
            "native_session_id": "native-1", "kind": "tool_result", "role": "tool", "content": "visible result"});
        if provenance {
            event["source_record_id"] = json!("record-1");
            event["metadata"] = json!({"tool_call_id": "call-1", "is_error": false});
        }
        let fixture = event.clone();
        let app = Router::new().route(
            &format!("/workstream/{workstream}/events"),
            get(move || {
                let event = fixture.clone();
                async move { Json(vec![event]) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let data = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        for structured in [false, true] {
            let mut command = crate::e2e_support::hermetic(env!("CARGO_BIN_EXE_ai-memory"));
            command
                .current_dir(cwd.path())
                .env("AI_MEMORY_HOME", home.path())
                .env("AI_MEMORY_DATA_DIR", data.path())
                .env("AI_MEMORY_SERVER_URL", &base)
                .env("RUST_LOG", "off")
                .args([
                    "workstream-search",
                    "--workstream-id",
                    workstream,
                    "visible",
                ]);
            if structured {
                command.arg("--json");
            }
            let output = tokio::process::Command::from(command)
                .output()
                .await
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8(output.stdout).unwrap();
            if structured {
                let events: Value = serde_json::from_str(&stdout).unwrap();
                assert_eq!(events[0]["source_record_id"], event["source_record_id"]);
                assert_eq!(events[0]["metadata"], event["metadata"]);
            } else {
                assert!(stdout.contains("visible result"));
                assert!(
                    stdout.contains("| codex | historical tool result (completed evidence)"),
                    "tool result not labelled completed evidence: {stdout}"
                );
                if provenance {
                    assert!(
                        stdout.contains("Source record: \"record-1\""),
                        "missing source record: {stdout}"
                    );
                    assert!(stdout.contains("\"tool_call_id\":\"call-1\""));
                } else {
                    assert!(!stdout.contains("Source record:"));
                }
            }
        }
        server.abort();
    }
}
