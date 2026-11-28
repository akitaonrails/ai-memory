//! `pending-writes list` consults a single scope, so an empty answer that does
//! not name it reads as "nothing to review anywhere".
use std::collections::HashMap;

use axum::{Json, Router, extract::Query, http::StatusCode, routing::get};
use serde_json::{Value, json};

/// Runs the real binary against a server that only answers for
/// `review/fixture`, so a wrong scope fails instead of looking empty.
async fn list(proposals: Value, cwd: &std::path::Path, args: &[&str]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().route(
        "/admin/pending-writes",
        get(move |Query(query): Query<HashMap<String, String>>| {
            let proposals = proposals.clone();
            async move {
                let in_scope = query.get("workspace").map(String::as_str) == Some("review")
                    && query.get("project").map(String::as_str) == Some("fixture");
                if in_scope {
                    Ok(Json(proposals))
                } else {
                    Err(StatusCode::BAD_REQUEST)
                }
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let mut cmd = crate::e2e_support::hermetic(env!("CARGO_BIN_EXE_ai-memory"));
    cmd.current_dir(cwd)
        .env("AI_MEMORY_HOME", home.path())
        .env("AI_MEMORY_DATA_DIR", data.path())
        .env("AI_MEMORY_SERVER_URL", format!("http://{addr}"))
        .args(["pending-writes", "list"])
        .args(args);
    let output = tokio::process::Command::from(cmd).output().await.unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

const EXPLICIT: &[&str] = &["--workspace", "review", "--project", "fixture"];

#[tokio::test]
async fn an_empty_list_names_the_explicit_scope() {
    let cwd = tempfile::tempdir().unwrap();
    let stdout = list(json!([]), cwd.path(), EXPLICIT).await;
    assert_eq!(stdout.trim(), "(no pending writes in review/fixture)");
}

/// The case that misled an operator: no flags, so the scope is one the command
/// resolved by itself.
#[tokio::test]
async fn an_empty_list_names_the_scope_it_resolved_without_flags() {
    let cwd = tempfile::tempdir().unwrap();
    std::fs::write(
        cwd.path().join(".ai-memory.toml"),
        "workspace = \"review\"\nproject = \"fixture\"\n",
    )
    .unwrap();
    let stdout = list(json!([]), cwd.path(), &[]).await;
    assert_eq!(stdout.trim(), "(no pending writes in review/fixture)");
}

/// Scripts read `--json`, so the scope stays out of it.
#[tokio::test]
async fn json_output_stays_a_bare_array() {
    let cwd = tempfile::tempdir().unwrap();
    let args = [EXPLICIT, &["--json"]].concat();
    let stdout = list(json!([]), cwd.path(), &args).await;
    assert_eq!(serde_json::from_str::<Value>(&stdout).unwrap(), json!([]));
}

#[tokio::test]
async fn a_populated_list_keeps_its_rows() {
    let cwd = tempfile::tempdir().unwrap();
    let proposal = json!({
        "id": "proposal-1",
        "status": "pending",
        "operation": "create",
        "target_path": "_rules/example.md",
        "kind": "rule",
        "title": "Example",
        "confidence": 0.9,
    });
    let stdout = list(json!([proposal]), cwd.path(), EXPLICIT).await;
    assert_eq!(
        stdout.trim(),
        "proposal-1  pending  create  _rules/example.md"
    );
}
