//! A `tool_response` object with no `text` / `content` field is stored as
//! compact JSON (`payload::value_to_text`), so its quotes and line breaks reach
//! the sanitizer as `\"` and `\n`; the rules must redact through them (#1963).

use ai_memory_core::{OBSERVATION_BODY_MAX_BYTES, ObservationKind, SessionId};
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
            "json file in a command code bash result",
            "command-code",
            serde_json::json!({"stdout": format!("{{\"db_password\": \"{SECRET}\"}}\n"), "stderr": "", "interrupted": false}),
            r#""interrupted":false"#,
        ),
        (
            "compact json file in a command code bash result",
            "command-code",
            serde_json::json!({"stdout": format!("{{\"SESSION_SECRET\":\"{SECRET}\"}}"), "stderr": "", "interrupted": false}),
            r#""interrupted":false"#,
        ),
        (
            "a .env past its first line in a command code bash result",
            "command-code",
            serde_json::json!({"stdout": format!("DB_HOST=x\nSTRIPE_SECRET={SECRET}\nPORT=5432\n"), "stderr": "", "interrupted": false}),
            r#"\nPORT=5432\n","stderr":"","interrupted":false"#,
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

/// A hook spool entry holds the static root bearer under a bare `token`
/// key; an agent printing one while debugging delivery must not store it
/// (#1982). Fails with the bare-`token` rule removed from the sanitizer.
#[tokio::test]
async fn a_printed_spool_entry_does_not_store_the_root_bearer() {
    let tmp = TempDir::new().unwrap();
    let state = make_state(&tmp).await;
    // A FAKE value in the `generate-auth-token` shape.
    let token = "0123456789abcdef".repeat(4);
    let entry = serde_json::json!({
        "url": "http://127.0.0.1:0/hook?event=post-tool-use",
        "body": "{}",
        "created_ms": 0,
        "auth_mode": "static",
        "token": token,
        "attempts": 0,
    });
    let tool_response = serde_json::json!({
        "stdout": format!("{entry}\n"),
        "stderr": "",
        "interrupted": false,
    });
    let body = stored_tool_body(&state, "claude-code", tool_response).await;
    assert!(
        !body.contains(&token) && body.contains("[REDACTED:"),
        "root bearer stored verbatim: {body:?}"
    );
    assert!(
        body.contains(r#"\"url\":\"http://127.0.0.1:0/hook"#)
            && body.contains(r#""interrupted":false"#),
        "redaction swallowed the rest of the entry: {body:?}"
    );
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
    let body = stored_tool_body(&state, "command-code", tool_response).await;
    assert!(
        body.ends_with(&expected),
        "harmless result must be stored whole: {body:?}"
    );
}

async fn stored_selected_lifecycle_body(
    state: &HookState,
    agent: ai_memory_core::AgentKind,
    event: crate::payload::HookEvent,
    message: &str,
    requested: bool,
) -> (String, String) {
    let sid = SessionId::new();
    let mut body = serde_json::json!({"session_id": sid.to_string(), "cwd": "/repo"});
    let (event_name, kind, selected) = if event == crate::payload::HookEvent::Stop {
        body["last_assistant_message"] = message.into();
        assert!(crate::assistant_capture::transform_for_client(&mut body, agent, event).captured);
        let selected = body[crate::assistant_capture::ASSISTANT_MARKER_KEY]["excerpt"]
            .as_str()
            .unwrap()
            .to_owned();
        ("stop", ObservationKind::Stop, selected)
    } else {
        let field = if agent == ai_memory_core::AgentKind::QwenCode {
            "submitted_prompt"
        } else {
            "prompt"
        };
        body[field] = message.into();
        assert!(crate::payload::cap_lifecycle_body_for_client(
            &mut body, event
        ));
        (
            "user-prompt",
            ObservationKind::UserPrompt,
            body[field].as_str().unwrap().to_owned(),
        )
    };
    let query_flag = if requested {
        "&capture_assistant=1"
    } else {
        ""
    };
    let agent_name = agent.as_str();
    let req = Request::builder().method("POST")
        .uri(format!("/hook?event={event_name}&agent={agent_name}&cwd=/repo&workspace=default&project=scratch{query_flag}"))
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap())).unwrap();
    let response = hook_router(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let stored = wait_for_kinds(state, &sid.to_string(), &[kind])
        .await
        .into_iter()
        .find(|o| o.kind == kind)
        .expect("selected lifecycle observation stored")
        .body;
    (selected, stored)
}

#[tokio::test]
async fn selected_lifecycle_ends_survive_the_unchanged_router_and_store() {
    use crate::payload::HookEvent;
    use ai_memory_core::{AgentKind, SanitizeConfig, Sanitizer};

    let tmp = TempDir::new().unwrap();
    let mut state = make_state(&tmp).await;
    assert!(state.consolidator.is_none());
    state.capture_assistant_enabled = true;
    state.sanitizer = Sanitizer::new(&SanitizeConfig {
        extra_patterns: vec!["SERVER_ONLY_PRIVATE".into()],
        allowlist: Vec::new(),
    })
    .unwrap();
    for agent in [
        AgentKind::ClaudeCode,
        AgentKind::Codex,
        AgentKind::OpenCode,
        AgentKind::QwenCode,
        AgentKind::KimiCode,
    ] {
        for (event, cap) in [(HookEvent::Stop, 2_000), (HookEvent::UserPrompt, 16_384)] {
            if event == HookEvent::Stop
                && !matches!(
                    agent,
                    AgentKind::ClaudeCode | AgentKind::Codex | AgentKind::OpenCode
                )
            {
                continue;
            }
            let message = format!(
                "INITIAL_DECISION\n{}\nSERVER_ONLY_PRIVATE FINAL_CORRECTION",
                "é🧪".repeat(cap / 3)
            );
            let (selected, stored) =
                stored_selected_lifecycle_body(&state, agent, event, &message, true).await;
            assert!(selected.contains("SERVER_ONLY_PRIVATE"));
            assert!(
                !stored.contains("SERVER_ONLY_PRIVATE"),
                "server sanitizer bypassed"
            );
            assert!(stored.contains("[REDACTED:"));
            assert!(
                stored.starts_with("INITIAL_DECISION"),
                "{agent:?}/{event:?}"
            );
            assert!(stored.ends_with("FINAL_CORRECTION"), "{agent:?}/{event:?}");
            assert!(stored.contains("...[truncated "));
            // `stored` is trimmed to the configured server cap; `selected` is
            // the client excerpt, bounded by the 16 KiB client ceiling and the
            // server cap only applies at the persistence boundary (#2118). Both
            // retain head and tail, which the start/end assertions above prove.
            assert!(stored.len() <= cap, "{agent:?}/{event:?} stored over cap");
            assert!(
                selected.len() <= OBSERVATION_BODY_MAX_BYTES,
                "{agent:?}/{event:?} selected over the client ceiling"
            );
        }
    }
}

#[tokio::test]
async fn selected_stop_still_requires_both_server_and_client_opt_ins() {
    for (server, requested) in [(false, false), (false, true), (true, false), (true, true)] {
        let tmp = TempDir::new().unwrap();
        let mut state = make_state(&tmp).await;
        state.capture_assistant_enabled = server;
        let message = format!("INITIAL_DECISION\n{}\nFINAL_RESULT", "x".repeat(4_000));
        let (selected, stored) = stored_selected_lifecycle_body(
            &state,
            ai_memory_core::AgentKind::Codex,
            crate::payload::HookEvent::Stop,
            &message,
            requested,
        )
        .await;
        if server && requested {
            // Both opt-ins on: the excerpt is stored with both ends retained.
            // The client sends up to the 16 KiB ceiling and the server trims to
            // the configured cap at the persistence boundary (#2118), so
            // `stored` is a head+tail excerpt of `selected`, not equal to it.
            assert!(!stored.is_empty());
            assert!(stored.starts_with("INITIAL_DECISION") && stored.ends_with("FINAL_RESULT"));
            assert!(selected.starts_with("INITIAL_DECISION") && selected.ends_with("FINAL_RESULT"));
            assert!(
                stored.len() <= selected.len() && stored.len() <= state.assistant_excerpt_max_bytes
            );
        } else {
            assert!(stored.is_empty(), "server={server} requested={requested}");
        }
    }
}
