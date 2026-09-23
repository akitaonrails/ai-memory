//! A4 entropy / boilerplate pre-filter, wired into the cross-session experience
//! pass (docs/design-memory-aging.md §A4).
//!
//! The gate runs BEFORE consolidation ingest: a low-information session page is
//! never placed in the consolidation prompt (and thus never reaches the eval
//! gate or `apply_batch`), while a real one is. Default (OFF) changes nothing.
//! The proof captures the prompt the LLM would receive and asserts which session
//! bodies made it in — skipping is advisory (the page is not deleted, invariant
//! #16).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use ai_memory_consolidate::entropy_filter::EntropyFilterConfig;
use ai_memory_consolidate::{AutoImproveReviewConfig, ExperienceConfig, run_experience_review};
use ai_memory_core::{ActorContext, NewSession, SessionId};
use ai_memory_llm::{ChatRequest, ChatResponse, LlmProvider, LlmResult};
use ai_memory_store::Store;
use ai_memory_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

/// LLM stub that records every prompt it is asked to complete, so the test can
/// assert which session bodies survived the entropy gate. Returns an empty (but
/// valid) proposal set.
struct CapturingLlm {
    prompts: Arc<Mutex<Vec<String>>>,
}

impl LlmProvider for CapturingLlm {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn model(&self) -> &str {
        "capturing"
    }
    fn complete<'life0, 'async_trait>(
        &'life0 self,
        _request: ChatRequest,
    ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            Ok(ChatResponse {
                text: "unused".into(),
                usage: None,
                model: "capturing".into(),
            })
        })
    }
    fn complete_structured_raw<'life0, 'async_trait>(
        &'life0 self,
        request: ChatRequest,
        _schema: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let prompts = self.prompts.clone();
        Box::pin(async move {
            if let Some(msg) = request.messages.first() {
                prompts.lock().unwrap().push(msg.content.clone());
            }
            Ok(serde_json::json!({
                "summary": "none",
                "proposals": [],
                "rejected_candidates": []
            }))
        })
    }
}

// A repetitive boilerplate status blob — no durable facts.
const BOILERPLATE: &str = "status ok status ok status ok status ok status ok status ok status ok";
// A real, terse-but-informative note with a file path and an error code.
const REAL: &str = "Fixed the retention sweep in crates/ai-memory-store/src/ops.rs which raised \
     E0433; tag main then deploy the stack.";

struct Harness {
    _tmp: TempDir,
    store: Store,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    prompts: Arc<Mutex<Vec<String>>>,
    llm: Arc<dyn LlmProvider>,
}

async fn harness_with_two_sessions() -> Harness {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .unwrap();

    for body in [BOILERPLATE, REAL] {
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: ai_memory_core::AgentKind::OpenCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store.writer.end_session(session_id, None).await.unwrap();
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: ai_memory_core::PagePath::new(format!("sessions/{session_id}.md")).unwrap(),
            frontmatter: serde_json::json!({"title": "session"}),
            body: body.to_string(),
            tier: ai_memory_core::Tier::Episodic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    }

    let prompts = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        prompts: prompts.clone(),
    });
    Harness {
        _tmp: tmp,
        store,
        ws,
        proj,
        prompts,
        llm,
    }
}

fn experience_cfg(filter_enabled: bool) -> ExperienceConfig {
    ExperienceConfig {
        sessions: 10,
        min_new_sessions: 1,
        entropy_filter: EntropyFilterConfig {
            enabled: filter_enabled,
            ..EntropyFilterConfig::default()
        },
        ..ExperienceConfig::default()
    }
}

/// Filter ON: the boilerplate session page is skipped from the prompt; the real
/// one is consolidated.
#[tokio::test]
async fn boilerplate_session_is_skipped_from_consolidation() {
    let h = harness_with_two_sessions().await;
    let report = run_experience_review(
        &h.store.reader,
        h.llm.as_ref(),
        h.ws,
        h.proj,
        AutoImproveReviewConfig::default(),
        &experience_cfg(true),
    )
    .await
    .unwrap();

    let prompts = h.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1, "the LLM was still called once");
    let prompt = &prompts[0];
    assert!(
        prompt.contains("crates/ai-memory-store/src/ops.rs"),
        "the real note is consolidated: {prompt}"
    );
    assert!(
        !prompt.contains("status ok status ok"),
        "the boilerplate note is skipped from the prompt: {prompt}"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.contains("entropy filter skipped")),
        "the skip is observable in the report: {:?}",
        report.warnings
    );
}

/// Default (OFF): nothing is filtered — both session pages reach the prompt, so
/// an upgrade changes no consolidation output.
#[tokio::test]
async fn default_off_changes_nothing() {
    let h = harness_with_two_sessions().await;
    run_experience_review(
        &h.store.reader,
        h.llm.as_ref(),
        h.ws,
        h.proj,
        AutoImproveReviewConfig::default(),
        &experience_cfg(false),
    )
    .await
    .unwrap();

    let prompts = h.prompts.lock().unwrap();
    assert_eq!(prompts.len(), 1);
    let prompt = &prompts[0];
    assert!(
        prompt.contains("crates/ai-memory-store/src/ops.rs"),
        "real note present: {prompt}"
    );
    assert!(
        prompt.contains("status ok status ok"),
        "with the filter off the boilerplate is NOT skipped: {prompt}"
    );
}
