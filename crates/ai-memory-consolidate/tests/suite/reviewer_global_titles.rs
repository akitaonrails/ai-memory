//! `[auto_improve] include_global_titles` (#1212): the reviewer may see the
//! reserved `_global` scope's titles, read-only, and nothing else outside its
//! own project.
//!
//! Adversarial: a sibling project's page must never reach the prompt or the
//! duplicate check, with the option on or off; `_global` reaches them only
//! with the option on. Each refusal has a legitimate control next to it.

use std::sync::{Arc, Mutex};

use ai_memory_core::{
    ActorContext, AgentKind, NewObservation, NewSession, ObservationKind, PagePath, ProjectId,
    Sanitized, Sanitizer, SessionId, Tier, WorkspaceId,
};
use ai_memory_llm::{ChatRequest, ChatResponse, LlmProvider, LlmResult};
use ai_memory_store::{ResolvedScope, Store};
use ai_memory_wiki::{Wiki, WritePageRequest};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::{
    AutoImproveReport, AutoImproveReviewConfig, ExperienceConfig, run_auto_improve_review,
    run_experience_review,
};

const GLOBAL_TITLE: &str = "Issues and pull requests carry no agent footer";
const SIBLING_TITLE: &str = "Sibling project keeps its deploy keys in a vault";

/// Records the prompt and proposes three pages: one at a `_global` page's
/// path, one under a `_global` page's title, one under a sibling project's
/// title, plus a control that duplicates nothing.
struct CapturingLlm {
    prompt: Arc<Mutex<String>>,
}

#[async_trait::async_trait]
impl LlmProvider for CapturingLlm {
    fn name(&self) -> &'static str {
        "fixture"
    }
    fn model(&self) -> &str {
        "fixture"
    }
    async fn complete(&self, _: ChatRequest) -> LlmResult<ChatResponse> {
        unreachable!("structured only")
    }
    async fn complete_structured_raw(&self, request: ChatRequest, _: Value) -> LlmResult<Value> {
        *self.prompt.lock().unwrap() = request.messages[0].content.clone();
        let page = |path: &str, title: &str| {
            json!({
                "path": path, "kind": "rule", "title": title,
                "confidence": 0.95, "rationale": "stated in the session",
                "body_markdown": format!("# {title}\n\nOne durable lesson."),
                "evidence": [{"page": "session", "quote": "keep agent footers out of pull requests"}]
            })
        };
        Ok(json!({
            "summary": "four candidates",
            "proposals": [
                page("_rules/git-attribution.md", "Commit trailers"),
                page("_rules/no-footer.md", GLOBAL_TITLE),
                page("_rules/vault.md", SIBLING_TITLE),
                page("_rules/release-notes.md", "Release notes name the ticket"),
            ]
        }))
    }
}

struct Fixture {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
}

async fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    Fixture {
        _tmp: tmp,
        store,
        wiki,
        ws,
    }
}

async fn write(fx: &Fixture, project: ProjectId, path: &str, title: &str) {
    fx.wiki
        .write_page(WritePageRequest {
            workspace_id: fx.ws,
            project_id: project,
            path: PagePath::new(path).unwrap(),
            frontmatter: json!({}),
            body: format!("# {title}\n\nA standing rule.\n"),
            tier: Tier::Semantic,
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

async fn session(fx: &Fixture, project: ProjectId) -> SessionId {
    let session_id = SessionId::new();
    fx.store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session_id,
            workspace_id: fx.ws,
            project_id: project,
            agent_kind: AgentKind::Other,
            cwd: None,
            actor_user: None,
            origin: None,
        })
        .await
        .unwrap();
    fx.store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                occurred_at: None,
                session_id,
                workspace_id: fx.ws,
                project_id: project,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "rule".into(),
                body: "keep agent footers out of pull requests".into(),
                importance: 5,
                file_path: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    session_id
}

async fn review(
    fx: &Fixture,
    project: ProjectId,
    include_global_titles: bool,
) -> (AutoImproveReport, String) {
    let session_id = session(fx, project).await;
    let prompt = Arc::new(Mutex::new(String::new()));
    let llm = CapturingLlm {
        prompt: prompt.clone(),
    };
    let cfg = AutoImproveReviewConfig {
        min_observations: 1,
        min_session_duration_secs: 0,
        include_global_titles,
        ..AutoImproveReviewConfig::default()
    };
    let report = run_auto_improve_review(&fx.store.reader, &llm, fx.ws, project, session_id, cfg)
        .await
        .unwrap();
    let prompt = prompt.lock().unwrap().clone();
    (report, prompt)
}

fn accepted(report: &AutoImproveReport) -> Vec<&str> {
    report.proposals.iter().map(|p| p.path.as_str()).collect()
}

fn rejection(report: &AutoImproveReport, path: &str) -> Option<String> {
    report
        .rejected_candidates
        .iter()
        .find(|r| r.target_path.as_deref() == Some(path))
        .map(|r| r.reason.clone())
}

/// Three projects in one workspace: the reviewed one, `_global`, and a
/// sibling holding a page the reviewer must never learn about.
async fn three_scopes(fx: &Fixture) -> (ProjectId, ResolvedScope) {
    let reviewed = fx
        .store
        .writer
        .get_or_create_project(fx.ws, "reviewed", None)
        .await
        .unwrap();
    let sibling = fx
        .store
        .writer
        .get_or_create_project(fx.ws, "sibling", None)
        .await
        .unwrap();
    let global = ai_memory_store::create_global_scope(&fx.store.writer)
        .await
        .unwrap();
    write(
        fx,
        global.project_id,
        "_rules/git-attribution.md",
        "Git Attribution",
    )
    .await;
    write(fx, global.project_id, "_rules/footer.md", GLOBAL_TITLE).await;
    write(fx, sibling, "_rules/vault.md", SIBLING_TITLE).await;
    (reviewed, global)
}

#[tokio::test]
async fn global_titles_reach_the_reviewer_only_when_enabled_and_never_a_sibling() {
    let fx = fixture().await;
    let (reviewed, _) = three_scopes(&fx).await;

    let (on, on_prompt) = review(&fx, reviewed, true).await;
    assert!(on_prompt.contains("reserved _global scope"));
    assert!(on_prompt.contains("_rules/git-attribution.md | Git Attribution"));
    assert!(on_prompt.contains(GLOBAL_TITLE));
    assert!(
        !on_prompt.contains(SIBLING_TITLE) && !on_prompt.contains("_rules/vault.md"),
        "a sibling project's page reached the reviewer"
    );
    assert_eq!(
        rejection(&on, "_rules/git-attribution.md").as_deref(),
        Some("duplicate_global_path")
    );
    assert_eq!(
        rejection(&on, "_rules/no-footer.md").as_deref(),
        Some("duplicate_global_title")
    );
    // The sibling stays invisible to the duplicate check too, and the
    // control duplicates nothing: both are accepted.
    let on_accepted = accepted(&on);
    assert!(on_accepted.contains(&"_rules/vault.md"));
    assert!(on_accepted.contains(&"_rules/release-notes.md"));

    // Off: no section, no `_global` refusal. The same proposals that the
    // guard refused above now pass, which is what the option changes.
    let (off, off_prompt) = review(&fx, reviewed, false).await;
    assert!(!off_prompt.contains("reserved _global scope"));
    assert!(!off_prompt.contains(GLOBAL_TITLE));
    assert!(!off_prompt.contains(SIBLING_TITLE));
    let off_accepted = accepted(&off);
    for path in [
        "_rules/git-attribution.md",
        "_rules/no-footer.md",
        "_rules/vault.md",
        "_rules/release-notes.md",
    ] {
        assert!(
            off_accepted.contains(&path),
            "{path} refused with the option off"
        );
    }
}

#[tokio::test]
async fn reviewing_global_itself_adds_no_global_section() {
    let fx = fixture().await;
    let (_, global) = three_scopes(&fx).await;

    let (report, prompt) = review(&fx, global.project_id, true).await;
    // `_global`'s own pages are its project pages: listed once, as such, and
    // refused by the ordinary project check rather than the `_global` one.
    assert!(!prompt.contains("reserved _global scope"));
    assert!(prompt.contains("_rules/git-attribution.md | Git Attribution"));
    assert!(!prompt.contains(SIBLING_TITLE));
    assert_eq!(
        rejection(&report, "_rules/git-attribution.md").as_deref(),
        Some("duplicate_existing_path")
    );
}

#[tokio::test]
async fn a_missing_global_scope_is_not_an_error() {
    let fx = fixture().await;
    let reviewed = fx
        .store
        .writer
        .get_or_create_project(fx.ws, "reviewed", None)
        .await
        .unwrap();

    let (report, prompt) = review(&fx, reviewed, true).await;
    assert!(!prompt.contains("reserved _global scope"));
    assert_eq!(report.proposals.len(), 4);
}

/// The cross-session experience pass reads the same option the same way.
#[tokio::test]
async fn the_experience_pass_follows_the_same_option() {
    let fx = fixture().await;
    let (reviewed, _) = three_scopes(&fx).await;
    for _ in 0..2 {
        let session_id = session(&fx, reviewed).await;
        fx.store.writer.end_session(session_id, None).await.unwrap();
        fx.wiki
            .write_page(WritePageRequest {
                workspace_id: fx.ws,
                project_id: reviewed,
                path: PagePath::new(format!("sessions/{session_id}.md")).unwrap(),
                frontmatter: json!({"title": "session"}),
                body: "Agreed to keep agent footers out of pull requests.".into(),
                tier: Tier::Episodic,
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
    let experience = ExperienceConfig {
        sessions: 10,
        min_new_sessions: 1,
        ..ExperienceConfig::default()
    };
    for include_global_titles in [true, false] {
        let prompt = Arc::new(Mutex::new(String::new()));
        let llm = CapturingLlm {
            prompt: prompt.clone(),
        };
        let cfg = AutoImproveReviewConfig {
            include_global_titles,
            ..AutoImproveReviewConfig::default()
        };
        let report =
            run_experience_review(&fx.store.reader, &llm, fx.ws, reviewed, cfg, &experience)
                .await
                .unwrap();
        let prompt = prompt.lock().unwrap().clone();
        assert!(
            !prompt.contains(SIBLING_TITLE),
            "a sibling project's page reached the reviewer"
        );
        assert_eq!(prompt.contains(GLOBAL_TITLE), include_global_titles);
        assert_eq!(
            rejection(&report, "_rules/git-attribution.md").as_deref()
                == Some("duplicate_global_path"),
            include_global_titles
        );
        assert!(accepted(&report).contains(&"_rules/release-notes.md"));
    }
}
