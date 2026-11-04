//! The opt-in residue reaches the provider but never bypasses structured output.

use std::sync::{Arc, Mutex};

use ai_memory_core::{
    ActorContext, AgentKind, NewObservation, NewSession, ObservationKind, Sanitized, Sanitizer,
    SessionId,
};
use ai_memory_llm::{ChatRequest, ChatResponse, LlmProvider, LlmResult};
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::Consolidator;

struct CapturingLlm {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait::async_trait]
impl LlmProvider for CapturingLlm {
    fn name(&self) -> &'static str {
        "capturing"
    }

    fn model(&self) -> &str {
        "capturing"
    }

    async fn complete(&self, _: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("consolidation uses structured completion")
    }

    async fn complete_structured_raw(&self, request: ChatRequest, _: Value) -> LlmResult<Value> {
        self.requests.lock().unwrap().push(request);
        Ok(json!({
            "title": "Fixed page",
            "body_markdown": "The provider controls this page body.",
            "tags": []
        }))
    }
}

#[tokio::test]
async fn residue_changes_only_the_provider_request_not_the_stored_page() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let project = store
        .writer
        .get_or_create_project(ws, "residue", None)
        .await
        .unwrap();
    let session = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session,
            workspace_id: ws,
            project_id: project,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
        })
        .await
        .unwrap();

    let sanitizer = Sanitizer::builtin();
    for idx in 0..302 {
        let (kind, title, body, importance) = match idx {
            0 => (
                ObservationKind::SessionStart,
                "start".to_string(),
                "cwd".to_string(),
                1,
            ),
            1 => (
                ObservationKind::PostToolUse,
                "routine output".to_string(),
                "tool_family: shell\n---\nsrc/only-in-omitted.rs".to_string(),
                1,
            ),
            300 => (
                ObservationKind::UserPrompt,
                "long user request".to_string(),
                format!("{}\nERROR: selected-tail-diagnostic", "x".repeat(3_050)),
                5,
            ),
            301 => (
                ObservationKind::SessionEnd,
                "end".to_string(),
                "done".to_string(),
                1,
            ),
            _ => (
                ObservationKind::PostToolUse,
                "routine output".to_string(),
                "ok".to_string(),
                1,
            ),
        };
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    occurred_at: None,
                    session_id: session,
                    workspace_id: ws,
                    project_id: project,
                    kind,
                    extension: None,
                    source_event: None,
                    title,
                    body,
                    importance,
                    file_path: None,
                },
                &sanitizer,
            ))
            .await
            .unwrap();
    }

    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests.clone(),
    });

    let off = Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        llm.clone(),
        ws,
        project,
    )
    .consolidate_session(session, false, ActorContext::anonymous(), None, None)
    .await
    .unwrap();
    let on = Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki,
        llm,
        ws,
        project,
    )
    .with_verbatim_residue(true)
    .consolidate_session(session, false, ActorContext::anonymous(), None, None)
    .await
    .unwrap();

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 2);
    let off_prompt = &captured[0].messages[0].content;
    let on_prompt = &captured[1].messages[0].content;
    assert!(!off_prompt.contains("src/only-in-omitted.rs"));
    assert!(!off_prompt.contains("selected-tail-diagnostic"));
    assert!(on_prompt.contains("  src/only-in-omitted.rs\n"));
    assert!(on_prompt.contains("  ERROR: selected-tail-diagnostic\n"));
    assert_eq!(captured[0].system, captured[1].system);
    assert_eq!(off.new_title, on.new_title);
    assert_eq!(off.new_body_markdown, on.new_body_markdown);
}
