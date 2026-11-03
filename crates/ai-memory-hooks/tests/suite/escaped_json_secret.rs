//! A `tool_response` object with no `text` / `content` field is stored as
//! compact JSON (`payload::value_to_text`), so its quotes and line breaks reach
//! the sanitizer as `\"` and `\n`; the rules must redact through them (#1963).

use ai_memory_core::{ObservationKind, SessionId};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use tempfile::TempDir;
use tower::ServiceExt;

use crate::router::tests::{make_state, wait_for_kinds};
use crate::router::{HookState, hook_router};

/// Posts one `PostToolUse` carrying `tool_response` and returns the stored body.
async fn stored_tool_body(
    state: &HookState,
    agent: &str,
    tool_response: serde_json::Value,
) -> String {
    let sid = SessionId::new();
    let body = serde_json::json!({
        "session_id": sid.to_string(),
        "cwd": "/repo",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "cat config"},
        "tool_response": tool_response,
        "tool_use_id": "toolu_01ABC124",
    });
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/hook?event=post-tool-use&agent={agent}&cwd=/repo&workspace=default&project=scratch"
        ))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let status = hook_router(state.clone())
        .oneshot(req)
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::ACCEPTED);

    wait_for_kinds(state, &sid.to_string(), &[ObservationKind::PostToolUse])
        .await
        .into_iter()
        .find(|o| o.kind == ObservationKind::PostToolUse)
        .expect("tool observation never landed")
        .body
}

const SECRET: &str = "correct-horse-battery";

/// Every object-shaped result, whatever harness or nesting, takes the
/// stringify branch. `survivor` is text after the secret that the redaction
/// must leave in place.
#[tokio::test]
async fn every_object_shaped_tool_result_is_redacted_in_the_stored_tool_body() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;
    for (label, agent, tool_response, survivor) in [
        (
            "json file in a claude code bash result",
            "claude-code",
            serde_json::json!({"stdout": format!("{{\"db_password\": \"{SECRET}\"}}\n"), "stderr": "", "interrupted": false}),
            r#""interrupted":false"#,
        ),
        (
            "stdout only, grok aliases",
            "grok",
            serde_json::json!({"stdout": format!("{{\"api_key\": \"{SECRET}\"}}\n"), "truncated": false}),
            r#""truncated":false"#,
        ),
        (
            "result nested one object deeper",
            "claude-code",
            serde_json::json!({"result": {"body": format!("{{\"db_password\": \"{SECRET}\"}}"), "path": "/repo/config.json"}}),
            r#""path":"/repo/config.json""#,
        ),
    ] {
        let body = stored_tool_body(&state, agent, tool_response).await;
        assert!(
            !body.contains(SECRET) && body.contains("[REDACTED:"),
            "{label}: secret stored verbatim: {body:?}"
        );
        assert!(
            body.contains(survivor),
            "{label}: redaction swallowed the rest of the result: {body:?}"
        );
    }
}

/// A result with no secret is stored as it arrived.
#[tokio::test]
async fn an_object_shaped_tool_result_without_a_secret_is_stored_whole() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;
    let tool_response = serde_json::json!({
        "stdout": "{\"name\": \"something-long-value\", \"Idempotency-Key\": \"00000000-0000-0000-0000-000000000000\"}\n",
        "stderr": "",
        "interrupted": false,
    });
    let expected = serde_json::to_string(&tool_response).unwrap();
    let body = stored_tool_body(&state, "claude-code", tool_response).await;
    assert!(
        body.ends_with(&expected),
        "harmless result must be stored whole: {body:?}"
    );
}
