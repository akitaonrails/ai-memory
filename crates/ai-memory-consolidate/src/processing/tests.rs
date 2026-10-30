//! Behavioural tests of the chunked engine over a real store and wiki with a
//! scripted provider that reads the same prompts a real model would.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use ai_memory_core::chunking::ChunkingSettings;
use ai_memory_core::{
    ActorContext, PagePath, ProcessingJobKind, ProcessingJobState, ProjectId, SessionId,
    WorkspaceId,
};
use ai_memory_llm::{ChatRequest, LlmError, LlmProvider, LlmResult, before_generation_send};
use ai_memory_store::{Store, WriterHandle};
use ai_memory_wiki::Wiki;
use serde_json::{Value, json};

use super::chunk_plan::text_cost;
use super::run::MAX_ERROR_RETRIES;
use super::{
    ProcessingCaller, ProcessingError, ProcessingJobAccess, ProcessingJobStart, ProcessingService,
};
use crate::AutoImproveReviewConfig;

const MARKERS: [&str; 3] = ["MARK-BEGIN", "MARK-MIDDLE", "MARK-END"];

/// One observed provider call.
#[derive(Clone)]
struct Call {
    stage: String,
    cost: usize,
}

/// A provider that plays the three stages from the prompt text alone.
struct StageLlm {
    calls: Arc<Mutex<Vec<Call>>>,
    count: AtomicUsize,
    /// Fail the map call (truncated) while it shows more than one header.
    truncate_multi_header_maps: bool,
    /// Fail every final call with a deterministic provider error.
    fail_final: bool,
    /// Answer every final call as truncated (a recoverable failure).
    truncate_final: bool,
    /// Answer every final call with this exact JSON (a well-formed but wrong envelope).
    malformed_final: Option<Value>,
    /// Pause every job (as a flag-off restart would) when this call starts.
    pause_at: Option<(usize, WriterHandle)>,
}

impl StageLlm {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            count: AtomicUsize::new(0),
            truncate_multi_header_maps: false,
            fail_final: false,
            truncate_final: false,
            malformed_final: None,
            pause_at: None,
        }
    }
}

fn digest_items(user: &str) -> Vec<(String, Vec<String>)> {
    let mut items: Vec<(String, Vec<String>)> = Vec::new();
    for line in user.lines() {
        let Some(rest) = line.strip_prefix("- ") else {
            continue;
        };
        let parts: Vec<&str> = rest.split(" | ").collect();
        if parts.len() < 3 {
            continue;
        }
        let refs = parts[2]
            .trim_start_matches("refs=")
            .split(',')
            .map(str::to_string)
            .collect();
        items.push((parts[1].to_string(), refs));
    }
    items
}

#[async_trait::async_trait]
impl LlmProvider for StageLlm {
    fn name(&self) -> &'static str {
        "stage-llm"
    }
    fn model(&self) -> &str {
        "stage-llm"
    }
    async fn complete(&self, _r: ChatRequest) -> LlmResult<ai_memory_llm::ChatResponse> {
        unreachable!("chunked processing only uses structured completion")
    }
    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        _schema: Value,
    ) -> LlmResult<Value> {
        // Like a terminal provider: announce the one real send first; a
        // refusal never reaches the model and is not counted as a call.
        before_generation_send().await?;
        let n = self.count.fetch_add(1, Ordering::SeqCst) + 1;
        let system = request.system.clone().unwrap_or_default();
        let stage = system.lines().next().unwrap_or("").to_string();
        let user = request.messages[0].content.clone();
        self.calls.lock().unwrap().push(Call {
            stage: stage.clone(),
            cost: text_cost(&system) + text_cost(&user),
        });
        if let Some((at, writer)) = &self.pause_at
            && n == *at
        {
            writer.pause_processing_jobs().await.unwrap();
        }
        if stage.ends_with("MAP") {
            let fence = user
                .lines()
                .find_map(|l| l.strip_prefix("<<<"))
                .unwrap_or("")
                .to_string();
            let headers = user
                .lines()
                .filter(|l| l.starts_with(&format!("{fence} [")))
                .count();
            if self.truncate_multi_header_maps && headers > 1 {
                return Err(LlmError::TruncatedResponse {
                    model: "stage-llm".into(),
                    completion_tokens: None,
                });
            }
            let mut label = String::new();
            let mut items = Vec::new();
            for line in user.lines() {
                if line.starts_with(&format!("{fence} [")) {
                    label = line
                        .trim_start_matches(&format!("{fence} ["))
                        .split(']')
                        .next()
                        .unwrap_or("")
                        .to_string();
                    continue;
                }
                for token in line.split_whitespace() {
                    let token = token.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
                    if token.starts_with("MARK-") && !label.is_empty() {
                        items.push(json!({
                            "kind": "fact", "text": format!("saw {token}"),
                            "refs": [label.clone()], "quote": "", "supersedes": []
                        }));
                    }
                }
            }
            return Ok(json!({ "items": items, "conflicts": [] }));
        }
        if stage.ends_with("REDUCE") {
            let mut merged: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (text, refs) in digest_items(&user) {
                merged.entry(text).or_default().extend(refs);
            }
            let items: Vec<Value> = merged
                .into_iter()
                .map(|(text, refs)| {
                    json!({"kind":"fact","text":text,"refs":refs,"quote":"","supersedes":[]})
                })
                .collect();
            return Ok(json!({ "items": items, "conflicts": [] }));
        }
        if self.fail_final {
            return Err(LlmError::Auth("expired".into()));
        }
        if self.truncate_final {
            return Err(LlmError::TruncatedResponse {
                model: "stage-llm".into(),
                completion_tokens: None,
            });
        }
        if let Some(answer) = &self.malformed_final {
            return Ok(answer.clone());
        }
        let body: String = digest_items(&user)
            .into_iter()
            .map(|(t, r)| format!("{t} ({})", r.join(", ")))
            .collect::<Vec<_>>()
            .join("\n");
        if stage.ends_with("FINAL-BATCH") {
            let sid = user
                .lines()
                .find_map(|l| l.strip_prefix("Session id: "))
                .unwrap_or("")
                .trim()
                .to_string();
            return Ok(json!({
                "updates": [
                    {"path": format!("sessions/{sid}.md"), "tier": "episodic", "kind": "fact",
                     "title": "Chunked batch session", "body_markdown": body, "tags": [],
                     "entities": []},
                    {"path": "concepts/chunked-concept.md", "tier": "semantic", "kind": "fact",
                     "title": "Chunked concept", "body_markdown": "Durable concept.", "tags": [],
                     "entities": []}
                ],
                "rationale": "chunked"
            }));
        }
        Ok(json!({ "title": "Chunked session", "body_markdown": body, "tags": [] }))
    }
}

struct Fixture {
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
    session: SessionId,
    _tmp: tempfile::TempDir,
}

/// `n` observations; the first, middle and last carry a distinct marker;
/// every body is `body_bytes` of filler so the session spans many chunks.
async fn fixture(n: usize, body_bytes: usize) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
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
    let session = SessionId::new();
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let base = 1_700_000_000_000_000_i64;
    conn.execute(
        "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
         VALUES (?1, ?2, ?3, 'claude-code', '/w', ?4)",
        rusqlite::params![session.as_bytes(), ws.as_bytes(), proj.as_bytes(), base],
    )
    .unwrap();
    for i in 0..n {
        let mut id = [0u8; 16];
        id[8..16].copy_from_slice(&(i as u64 + 1).to_be_bytes());
        let marker = if i == 0 {
            MARKERS[0]
        } else if i == n / 2 {
            MARKERS[1]
        } else if i == n - 1 {
            MARKERS[2]
        } else {
            ""
        };
        let filler = "lorem ipsum dolor ".repeat(body_bytes / 18 + 1);
        let body = format!("{marker} step {i}\n\n{filler}");
        conn.execute(
            "INSERT INTO observations \
             (id, session_id, workspace_id, project_id, kind, title, body, created_at) \
             VALUES (?1, ?2, ?3, ?4, 'post-tool-use', ?5, ?6, ?7)",
            rusqlite::params![
                &id[..],
                session.as_bytes(),
                ws.as_bytes(),
                proj.as_bytes(),
                format!("tool {i}"),
                body,
                // pairs share a timestamp: order comes from the manifest, not time
                base + (i as i64 / 2) * 1_000_000,
            ],
        )
        .unwrap();
    }
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    Fixture {
        store,
        wiki,
        ws,
        proj,
        session,
        _tmp: tmp,
    }
}

fn service(
    f: &Fixture,
    llm: Arc<dyn LlmProvider>,
    enabled: bool,
    max_calls: u32,
) -> ProcessingService {
    ProcessingService::new(
        f.store.reader.clone(),
        f.store.writer.clone(),
        f.wiki.clone(),
        Some(llm),
        f.ws,
        f.proj,
        ChunkingSettings {
            enabled,
            max_input_tokens: 8192,
            max_calls_per_run: max_calls,
        },
    )
    .with_auto_improve(AutoImproveReviewConfig::default(), true)
}

fn start(f: &Fixture, kind: ProcessingJobKind) -> ProcessingJobStart {
    ProcessingJobStart {
        workspace_id: f.ws,
        project_id: f.proj,
        session_id: f.session,
        kind,
        actor: ActorContext::default(),
        author_id: None,
        owner: None,
        viewer: None,
        instructions: None,
    }
}

fn anchor(f: &Fixture) -> PagePath {
    PagePath::new(format!("sessions/{}.md", f.session)).unwrap()
}

#[tokio::test]
async fn a_long_session_is_read_whole_and_published_once() {
    let f = fixture(40, 3_000).await;
    let llm = Arc::new(StageLlm::new());
    let calls = llm.calls.clone();
    let svc = service(&f, llm, true, 128);
    svc.validate().expect("default budgets hold the whole path");

    let outcome = svc
        .consolidate_session(f.session, false, ProcessingCaller::default(), None)
        .await
        .unwrap();
    assert!(outcome.page_id.is_some());
    for marker in MARKERS {
        assert!(
            outcome.new_body_markdown.contains(marker),
            "{marker} from the start, middle and end must reach the page"
        );
    }
    let page = f.wiki.read_page(f.ws, f.proj, &anchor(&f)).unwrap();
    assert!(page.body.contains("MARK-MIDDLE"));

    {
        let calls = calls.lock().unwrap();
        let maps = calls.iter().filter(|c| c.stage.ends_with("MAP")).count();
        assert!(
            maps > 3,
            "a 120 KB session needs many bounded map calls, got {maps}"
        );
        assert_eq!(
            calls.iter().filter(|c| c.stage.contains("FINAL")).count(),
            1
        );
        for call in calls.iter() {
            assert!(
                call.cost <= 19_660,
                "every call stays inside the chunk budget, got {}",
                call.cost
            );
            assert!(
                call.stage.starts_with("ai-memory chunk stage:"),
                "no legacy scaffolding on the chunked path: {}",
                call.stage
            );
        }
    }

    // Same request again joins the completed job: no new calls, no new version.
    let _ = ();
    let again = svc
        .consolidate_session(f.session, false, ProcessingCaller::default(), None)
        .await
        .unwrap();
    assert_eq!(again.page_id, outcome.page_id);
    let _ = ();
}

#[tokio::test]
async fn multi_page_publication_goes_through_the_batch_guards() {
    let f = fixture(12, 2_500).await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    let outcomes = svc
        .consolidate_session_multi(f.session, false, ProcessingCaller::default(), None)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|o| o.page_id.is_some()));
    let concept = PagePath::new("concepts/chunked-concept.md").unwrap();
    assert!(f.wiki.read_page(f.ws, f.proj, &concept).is_ok());
    let session_page = f.wiki.read_page(f.ws, f.proj, &anchor(&f)).unwrap();
    assert_eq!(
        session_page.frontmatter["session_id"],
        f.session.to_string()
    );
    assert!(session_page.frontmatter.get("processing_job").is_some());
}

#[tokio::test]
async fn a_truncated_map_answer_is_subdivided_never_accepted_as_empty() {
    let f = fixture(6, 400).await;
    let mut llm = StageLlm::new();
    llm.truncate_multi_header_maps = true;
    let llm = Arc::new(llm);
    let calls = llm.calls.clone();
    let svc = service(&f, llm, true, 128);
    let outcome = svc
        .consolidate_session(f.session, false, ProcessingCaller::default(), None)
        .await
        .unwrap();
    for marker in MARKERS {
        assert!(outcome.new_body_markdown.contains(marker), "{marker} lost");
    }
    let maps = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.stage.ends_with("MAP"))
        .count();
    assert!(
        maps >= 6,
        "each observation was finally read on its own: {maps}"
    );
}

#[tokio::test]
async fn a_failed_final_call_publishes_nothing() {
    let f = fixture(10, 2_000).await;
    let mut llm = StageLlm::new();
    llm.fail_final = true;
    let svc = service(&f, Arc::new(llm), true, 128);
    let err = svc
        .consolidate_session(f.session, false, ProcessingCaller::default(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, crate::ConsolidatorError::Processing(_)),
        "{err:?}"
    );
    assert!(
        f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_err(),
        "no partial summary may be published"
    );
}

#[tokio::test]
async fn a_restart_reuses_completed_nodes_and_keeps_spent_calls() {
    let f = fixture(30, 3_000).await;
    // First process: the provider stops the job (as a flag-off start does)
    // while its 4th call is in flight.
    let mut first = StageLlm::new();
    first.pause_at = Some((4, f.store.writer.clone()));
    let first = Arc::new(first);
    let first_calls = first.calls.clone();
    let svc = service(&f, first, true, 128);
    let created = svc
        .start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();
    let after_pause = svc.execute_next().await.unwrap().unwrap();
    assert_eq!(after_pause.state, ProcessingJobState::Paused);
    assert_eq!(
        after_pause.calls_used, 4,
        "the in-flight attempt stays spent"
    );
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_err());

    // Second process: resumes the same job; completed nodes are not redone.
    let second = Arc::new(StageLlm::new());
    let second_calls = second.calls.clone();
    let svc2 = service(&f, second, true, 128);
    assert_eq!(
        f.store
            .writer
            .resume_paused_processing_jobs()
            .await
            .unwrap(),
        1
    );
    let done = svc2.execute_next().await.unwrap().unwrap();
    assert_eq!(done.job_id, created.job_id);
    assert_eq!(done.state, ProcessingJobState::Completed);
    let first_maps = first_calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.stage.ends_with("MAP"))
        .count();
    let total_maps = done.chunks_total as usize;
    let second_maps = second_calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.stage.ends_with("MAP"))
        .count();
    assert!(first_maps >= 3);
    assert!(
        second_maps < total_maps,
        "the {} completed map nodes must not run again ({second_maps} of {total_maps})",
        first_maps - 1
    );
    assert!(done.calls_used > after_pause.calls_used);
    let page = f.wiki.read_page(f.ws, f.proj, &anchor(&f)).unwrap();
    for marker in MARKERS {
        assert!(page.body.contains(marker));
    }
}

#[tokio::test]
async fn a_call_ceiling_that_cannot_cover_the_session_ends_before_any_call() {
    let f = fixture(40, 3_000).await;
    let llm = Arc::new(StageLlm::new());
    let calls = llm.calls.clone();
    let svc = service(&f, llm, true, 2);
    svc.start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();
    let status = svc.execute_next().await.unwrap().unwrap();
    assert_eq!(status.state, ProcessingJobState::BudgetExceeded);
    assert!(
        calls.lock().unwrap().is_empty(),
        "no call is spent on partial coverage"
    );
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_err());
}

#[tokio::test]
async fn flag_off_refuses_new_work_and_a_paused_job_blocks_the_legacy_path() {
    let f = fixture(20, 3_000).await;
    let off = service(&f, Arc::new(StageLlm::new()), false, 128);
    assert!(matches!(
        off.start(start(&f, ProcessingJobKind::Consolidate)).await,
        Err(ProcessingError::Disabled)
    ));

    // A job created while the flag was on, then paused by a flag-off start.
    let mut stopper = StageLlm::new();
    stopper.pause_at = Some((1, f.store.writer.clone()));
    let on = service(&f, Arc::new(stopper), true, 128);
    let job = on
        .start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();
    on.execute_next().await.unwrap();
    assert_eq!(off.pause_active_jobs().await.unwrap(), 0);

    // Old jobs stay observable with the flag off ...
    let seen = off
        .status(
            f.ws,
            f.proj,
            job.job_id,
            ProcessingJobAccess {
                any_owner: true,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .expect("status works while off");
    assert_eq!(seen.state, ProcessingJobState::Paused);
    // ... and the legacy path refuses instead of publishing beside them.
    let err = off
        .consolidate_session(f.session, false, ProcessingCaller::default(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, crate::ConsolidatorError::Processing(_)),
        "{err:?}"
    );
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_err());
}

#[tokio::test]
async fn a_job_id_does_not_cross_projects_or_owners() {
    let f = fixture(8, 1_000).await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    let job = svc
        .start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();
    let other_proj = f
        .store
        .writer
        .get_or_create_project(f.ws, "elsewhere", None)
        .await
        .unwrap();
    let root = ProcessingJobAccess {
        any_owner: true,
        ..Default::default()
    };
    assert!(
        svc.status(f.ws, other_proj, job.job_id, root.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        svc.status(f.ws, f.proj, job.job_id, root)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn auto_improve_merges_candidates_into_one_staged_run() {
    let f = fixture(24, 2_500).await;
    let svc = service(&f, Arc::new(ReviewLlm::new()), true, 128);
    let staged = svc
        .auto_improve_staged(super::AutoImproveStageRequest {
            workspace_id: f.ws,
            project_id: f.proj,
            session_id: f.session,
            cfg: AutoImproveReviewConfig {
                min_observations: 1,
                min_session_duration_secs: 0,
                ..AutoImproveReviewConfig::default()
            },
            actor: ActorContext::default(),
            author_id: None,
            viewer: None,
            owner: None,
            admission_ctx: None,
        })
        .await
        .unwrap();
    assert!(staged.approval_required);
    assert_eq!(
        staged.proposals.len(),
        1,
        "two candidates for one page stage one proposal: {:?}",
        staged.proposals
    );
    assert_eq!(staged.proposals[0].status, "pending");
    // joining the finished job returns the same staged ids, no second run
    let again = svc
        .auto_improve_staged(super::AutoImproveStageRequest {
            workspace_id: f.ws,
            project_id: f.proj,
            session_id: f.session,
            cfg: AutoImproveReviewConfig {
                min_observations: 1,
                min_session_duration_secs: 0,
                ..AutoImproveReviewConfig::default()
            },
            actor: ActorContext::default(),
            author_id: None,
            viewer: None,
            owner: None,
            admission_ctx: None,
        })
        .await
        .unwrap();
    assert_eq!(again.run_id, staged.run_id);
}

/// Answers map/reduce like [`StageLlm`] and the review with two candidates
/// for the same page.
struct ReviewLlm(StageLlm);

impl ReviewLlm {
    fn new() -> Self {
        Self(StageLlm::new())
    }
}

#[async_trait::async_trait]
impl LlmProvider for ReviewLlm {
    fn name(&self) -> &'static str {
        "review-llm"
    }
    fn model(&self) -> &str {
        "review-llm"
    }
    async fn complete(&self, r: ChatRequest) -> LlmResult<ai_memory_llm::ChatResponse> {
        self.0.complete(r).await
    }
    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: Value,
    ) -> LlmResult<Value> {
        let system = request.system.clone().unwrap_or_default();
        if !system.starts_with("ai-memory chunk stage: FINAL-AUTO-IMPROVE") {
            return self.0.complete_structured_raw(request, schema).await;
        }
        before_generation_send().await?;
        let proposal = |confidence: f64, text: &str| {
            json!({
                "operation": "create_or_update", "path": "gotchas/chunked-lessons.md",
                "title": "Chunked lessons", "kind": "gotcha", "confidence": confidence,
                "rationale": "seen across the session",
                "evidence": [{"page": "o0", "quote": text}],
                "body_markdown": format!("# Chunked lessons\n\n{text}"),
                "edit_mode": "full_page", "edits": []
            })
        };
        Ok(json!({
            "summary": "two lessons",
            "proposals": [proposal(0.8, "early lesson"), proposal(0.95, "late lesson")],
            "rejected_candidates": []
        }))
    }
}

async fn member(f: &Fixture, level: ai_memory_store::GrantLevel) -> ai_memory_core::UserId {
    f.store
        .writer
        .set_access_mode(f.proj, ai_memory_store::AccessMode::Restricted)
        .await
        .unwrap();
    let user = f
        .store
        .writer
        .create_human_user(
            ai_memory_core::NewUser {
                username: "alice".into(),
                name: None,
                email: None,
            },
            ai_memory_core::UserRole::User,
            None,
            false,
        )
        .await
        .unwrap();
    f.store
        .writer
        .grant_memory(user, f.proj, level, None)
        .await
        .unwrap();
    user
}

fn viewed_by(f: &Fixture, user: ai_memory_core::UserId) -> ProcessingJobStart {
    ProcessingJobStart {
        viewer: Some(user),
        author_id: Some(user),
        ..start(f, ProcessingJobKind::Consolidate)
    }
}

#[tokio::test]
async fn a_viewer_whose_grant_was_revoked_while_queued_never_reaches_the_provider() {
    let f = fixture(8, 1_000).await;
    let user = member(&f, ai_memory_store::GrantLevel::Write).await;
    let llm = Arc::new(StageLlm::new());
    let calls = llm.calls.clone();
    let svc = service(&f, llm, true, 128);
    svc.start(viewed_by(&f, user)).await.unwrap();
    f.store
        .writer
        .revoke_memory(user, f.proj, None)
        .await
        .unwrap();
    let status = svc.execute_next().await.unwrap().unwrap();
    assert_eq!(status.state, ProcessingJobState::Invalidated);
    assert!(
        calls.lock().unwrap().is_empty(),
        "no call after the revocation"
    );
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_err());
}

#[tokio::test]
async fn a_read_only_viewer_cannot_run_or_publish_a_write_job() {
    let f = fixture(8, 1_000).await;
    let user = member(&f, ai_memory_store::GrantLevel::Read).await;
    let llm = Arc::new(StageLlm::new());
    let calls = llm.calls.clone();
    let svc = service(&f, llm, true, 128);
    svc.start(viewed_by(&f, user)).await.unwrap();
    let status = svc.execute_next().await.unwrap().unwrap();
    assert_eq!(status.state, ProcessingJobState::Invalidated);
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_disabled_viewer_is_invalidated_before_any_call() {
    let f = fixture(8, 1_000).await;
    let user = member(&f, ai_memory_store::GrantLevel::Write).await;
    let llm = Arc::new(StageLlm::new());
    let calls = llm.calls.clone();
    let svc = service(&f, llm, true, 128);
    svc.start(viewed_by(&f, user)).await.unwrap();
    f.store.writer.set_user_disabled(user, true).await.unwrap();
    let status = svc.execute_next().await.unwrap().unwrap();
    assert_eq!(status.state, ProcessingJobState::Invalidated);
    assert!(calls.lock().unwrap().is_empty());
}

/// Claim the queued job and build its driver, the way `execute_next` does.
async fn claimed_run(f: &Fixture, svc: &ProcessingService) -> super::run::Run {
    let claimed = f
        .store
        .writer
        .claim_next_processing_job(std::time::Duration::from_secs(120))
        .await
        .unwrap()
        .expect("a queued job");
    let writer = f.store.writer.fenced(&claimed.claim);
    match super::run::Run::new(svc, claimed, writer, super::Overrides::default()) {
        Ok(run) => run,
        Err(_) => panic!("the claimed job must be runnable"),
    }
}

async fn planned_pages(run: &super::run::Run) -> super::publish::Plan {
    let ctx = match run.consolidate_context(false).await {
        Ok(ctx) => ctx,
        Err(_) => panic!("context"),
    };
    let page: crate::types::ConsolidatedPage = serde_json::from_value(json!({
        "title": "Crash page", "body_markdown": "Body written once.", "tags": []
    }))
    .unwrap();
    match run
        .plan_pages(super::final_stage::FinalRecord::Page { page }, &ctx)
        .await
    {
        Ok(plan) => plan,
        Err(_) => panic!("plan"),
    }
}

async fn latest(f: &Fixture) -> Option<ai_memory_core::PageId> {
    f.store
        .reader
        .latest_page_id_by_ids(f.ws, f.proj, anchor(f).as_str().to_string())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_crash_between_the_page_write_and_its_effect_adopts_instead_of_rewriting() {
    let f = fixture(6, 500).await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    svc.start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();
    let mut run = claimed_run(&f, &svc).await;
    let plan = planned_pages(&run).await;
    let super::publish::Plan::Pages(pages) = &plan else {
        panic!("a consolidation plans pages");
    };
    let publication = run
        .writer
        .begin_processing_publication(
            &run.claim,
            "crash:test".into(),
            serde_json::to_value(&plan).unwrap(),
        )
        .await
        .unwrap();
    // The write lands; the process dies before the effect is recorded.
    if run.write_pages(pages).await.is_err() {
        panic!("write");
    }
    let written = latest(&f).await.expect("the page was written");
    // The restarted tail must not write a second version.
    let status = match run.publish_resume(publication).await {
        Ok(status) => status,
        Err(_) => panic!("resume"),
    };
    assert_eq!(status.state, ProcessingJobState::Completed);
    assert_eq!(latest(&f).await, Some(written), "no second version");
}

#[tokio::test]
async fn a_recorded_effect_without_a_git_commit_is_finished_on_resume() {
    let f = fixture(6, 500).await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    svc.start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();
    let mut run = claimed_run(&f, &svc).await;
    let plan = planned_pages(&run).await;
    let super::publish::Plan::Pages(pages) = &plan else {
        panic!("a consolidation plans pages");
    };
    let publication = run
        .writer
        .begin_processing_publication(
            &run.claim,
            "crash:test".into(),
            serde_json::to_value(&plan).unwrap(),
        )
        .await
        .unwrap();
    let ids = match run.write_pages(pages).await {
        Ok(ids) => ids,
        Err(_) => panic!("write"),
    };
    run.writer
        .record_processing_effect(
            &run.claim,
            "pages".into(),
            json!({ "ids": ids.iter().map(ToString::to_string).collect::<Vec<_>>() }),
        )
        .await
        .unwrap();
    let status = match run.publish_resume(publication).await {
        Ok(status) => status,
        Err(_) => panic!("resume"),
    };
    assert_eq!(status.state, ProcessingJobState::Completed);
    assert_eq!(latest(&f).await, ids.first().copied());
}

// ---- staged auto-improve recovery ---------------------------------------

const STAGED_PATH: &str = "gotchas/staged-recovery.md";

fn staged_cfg() -> AutoImproveReviewConfig {
    AutoImproveReviewConfig::default()
}

fn staged_report(f: &Fixture) -> crate::auto_improve::AutoImproveReport {
    serde_json::from_value(json!({
        "session_id": f.session.to_string(),
        "observations_considered": 6,
        "session_duration_secs": 60,
        "estimated_input_tokens": 100,
        "provider": "stage-llm",
        "model": "stage-llm",
        "min_confidence": 0.7,
        "proposal_actor": "auto_improve",
        "pending_path": "_pending/auto-improve",
        "summary": "one lesson",
        "proposals": [{
            "operation": "create_or_update", "path": STAGED_PATH,
            "title": "Staged recovery", "kind": "gotcha", "confidence": 0.9,
            "rationale": "seen in the session",
            "evidence": [{"page": "o0", "quote": "recover once"}],
            "body_markdown": "# Staged recovery\n\nRecover once.",
            "edit_mode": "full_page", "edits": []
        }],
        "rejected_candidates": [],
        "warnings": []
    }))
    .unwrap()
}

fn staged_rows(f: &Fixture) -> (i64, i64) {
    let conn = rusqlite::Connection::open(f.store.db_path()).unwrap();
    let count = |table: &str| {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap()
    };
    (count("auto_improve_runs"), count("auto_improve_proposals"))
}

async fn recorded_effects(
    f: &Fixture,
    run: &super::run::Run,
) -> Vec<ai_memory_store::ProcessingEffect> {
    f.store
        .reader
        .processing_effects(f.ws, f.proj, run.job_id)
        .await
        .unwrap()
}

fn stage_request(f: &Fixture) -> super::AutoImproveStageRequest {
    super::AutoImproveStageRequest {
        workspace_id: f.ws,
        project_id: f.proj,
        session_id: f.session,
        cfg: staged_cfg(),
        actor: ActorContext::default(),
        author_id: None,
        viewer: None,
        owner: None,
        admission_ctx: None,
    }
}

/// A staging job whose run committed to the store but died before the
/// `stage` effect was checkpointed: the same plan, publication identity and
/// store call the live tail makes, minus the checkpoint.
async fn crashed_after_stage(
    f: &Fixture,
    svc: &ProcessingService,
) -> (
    super::run::Run,
    ai_memory_store::ProcessingPublication,
    ai_memory_core::AutoImproveRunId,
    Vec<ai_memory_core::AutoImproveProposalId>,
) {
    svc.enqueue(
        &start(f, ProcessingJobKind::AutoImprove),
        &super::StartOptions {
            instructions: None,
            review: Some(staged_cfg()),
            trigger: "durable",
        },
    )
    .await
    .unwrap();
    let run = claimed_run(f, svc).await;
    let plan = match run.plan_for_report(staged_report(f), &staged_cfg()).await {
        Ok(plan) => plan,
        Err(_) => panic!("plan"),
    };
    let super::publish::Plan::Stage(stage) = &plan else {
        panic!("an auto-improve review plans a staged run");
    };
    let input = match stage.stage_input(&run) {
        Ok(input) => input,
        Err(_) => panic!("stage input"),
    };
    let publication = run
        .writer
        .begin_processing_publication(
            &run.claim,
            "crash:stage".into(),
            serde_json::to_value(&plan).unwrap(),
        )
        .await
        .unwrap();
    let report = run
        .writer
        .stage_auto_improve_run_for_owner(input, run.owner.clone())
        .await
        .unwrap();
    (run, publication, report.run_id, report.proposal_ids)
}

async fn staged_recovery(require_approval: bool) {
    let f = fixture(6, 500).await;
    let llm = Arc::new(StageLlm::new());
    let calls = llm.calls.clone();
    let svc = service(&f, llm, true, 128).with_auto_improve(staged_cfg(), require_approval);
    let (mut run, publication, run_id, ids) = crashed_after_stage(&f, &svc).await;
    assert_eq!(ids.len(), 1, "one planned target stages one proposal");
    assert_eq!(staged_rows(&f), (1, 1));
    let sidecar = f.wiki.auto_improve_sidecar_path(f.ws, f.proj, ids[0]);
    assert!(!sidecar.exists(), "the crash left no sidecar");
    assert!(
        recorded_effects(&f, &run).await.is_empty(),
        "the crash left no checkpoint, no approval"
    );
    let page = PagePath::new(STAGED_PATH).unwrap();
    assert!(f.wiki.read_page(f.ws, f.proj, &page).is_err());

    let status = match run.publish_resume(publication).await {
        Ok(status) => status,
        Err(_) => panic!("resume"),
    };
    assert_eq!(status.state, ProcessingJobState::Completed);
    let staged = super::publish::staged_from_result(status.result.as_ref()).unwrap();
    assert_eq!(staged.run_id, run_id, "the exact staged run is adopted");
    assert_eq!(
        staged.proposals.iter().map(|p| p.id).collect::<Vec<_>>(),
        ids,
        "the exact staged proposals are adopted"
    );
    assert!(staged.skipped.is_empty());
    assert_eq!(staged.approval_required, require_approval);
    assert_eq!(staged_rows(&f), (1, 1), "no second run or proposal");
    assert!(sidecar.exists());
    assert_eq!(
        std::fs::read_dir(sidecar.parent().unwrap())
            .unwrap()
            .count(),
        1,
        "one sidecar, not a duplicate"
    );
    let effects = recorded_effects(&f, &run).await;
    let stage = effects
        .iter()
        .filter(|e| e.key == "stage")
        .collect::<Vec<_>>();
    assert_eq!(stage.len(), 1);
    assert_eq!(stage[0].json["run_id"], json!(run_id.to_string()));
    assert_eq!(
        stage[0].json["proposal_ids"],
        json!(ids.iter().map(ToString::to_string).collect::<Vec<_>>())
    );
    let approvals = effects
        .iter()
        .filter(|e| e.key.starts_with("approve:"))
        .count();
    let published = f.wiki.read_page(f.ws, f.proj, &page);
    if require_approval {
        assert_eq!(staged.proposals[0].status, "pending");
        assert!(staged.proposals[0].page_id.is_none());
        assert_eq!(approvals, 0, "the approval policy is not bypassed");
        assert!(published.is_err(), "nothing is published without approval");
    } else {
        assert_eq!(staged.proposals[0].status, "approved");
        assert_eq!(approvals, 1, "approved exactly once");
        assert!(published.is_ok());
    }
    let version = f
        .store
        .reader
        .latest_page_id_by_ids(f.ws, f.proj, STAGED_PATH.to_string())
        .await
        .unwrap();
    assert_eq!(
        version.map(|id| id.to_string()),
        staged.proposals[0].page_id.clone(),
        "the published version is the approved one"
    );

    // Joining the finished job returns the same staged ids, with no new
    // provider call, run or page version.
    let again = svc.auto_improve_staged(stage_request(&f)).await.unwrap();
    assert_eq!(again.run_id, run_id);
    assert_eq!(
        again.proposals.iter().map(|p| p.id).collect::<Vec<_>>(),
        ids
    );
    assert_eq!(staged_rows(&f), (1, 1));
    assert!(
        calls.lock().unwrap().is_empty(),
        "recovery never calls the provider"
    );
    let after = f
        .store
        .reader
        .latest_page_id_by_ids(f.ws, f.proj, STAGED_PATH.to_string())
        .await
        .unwrap();
    assert_eq!(after, version);
}

#[tokio::test]
async fn a_staged_run_committed_before_its_checkpoint_is_adopted_and_stays_pending() {
    staged_recovery(true).await;
}

#[tokio::test]
async fn a_staged_run_committed_before_its_checkpoint_is_approved_once_when_policy_allows() {
    staged_recovery(false).await;
}

// ---- concurrent edits and per-dispatch grants ----------------------------

async fn write_anchor(f: &Fixture, body: &str) -> ai_memory_core::PageId {
    f.wiki
        .write_page(ai_memory_wiki::WritePageRequest {
            workspace_id: f.ws,
            project_id: f.proj,
            path: anchor(f),
            frontmatter: json!({"title": "anchor"}),
            body: body.into(),
            tier: ai_memory_core::Tier::Episodic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn a_concurrent_edit_after_the_context_read_conflicts_and_is_never_the_base() {
    let f = fixture(6, 500).await;
    let original = write_anchor(&f, "original body").await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    svc.start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();
    let mut run = claimed_run(&f, &svc).await;
    let key = anchor(&f).as_str().to_string();

    // The final call reads context A ...
    let ctx_a = match run.consolidate_context(false).await {
        Ok(ctx) => ctx,
        Err(_) => panic!("context a"),
    };
    assert_eq!(ctx_a.bases[&key], Some(original.to_string()));
    // ... somebody else edits the page ...
    let edited = write_anchor(&f, "concurrent edit B").await;
    // ... and a later read sees B. It must not replace A as the base.
    let ctx_b = match run.consolidate_context(false).await {
        Ok(ctx) => ctx,
        Err(_) => panic!("context b"),
    };
    assert_eq!(ctx_b.bases[&key], Some(edited.to_string()));
    assert_ne!(ctx_a.bases[&key], ctx_b.bases[&key]);

    let page: crate::types::ConsolidatedPage = serde_json::from_value(json!({
        "title": "Crash page", "body_markdown": "Derived from A.", "tags": []
    }))
    .unwrap();
    let plan = match run
        .plan_pages(super::final_stage::FinalRecord::Page { page }, &ctx_a)
        .await
    {
        Ok(plan) => plan,
        Err(_) => panic!("plan"),
    };
    let publication = run
        .writer
        .begin_processing_publication(
            &run.claim,
            "conflict:test".into(),
            serde_json::to_value(&plan).unwrap(),
        )
        .await
        .unwrap();
    match run.publish_resume(publication).await {
        Err(super::run::Abort::Finish(ai_memory_store::ProcessingOutcome::Failed { error })) => {
            assert!(
                error.contains("changed"),
                "a conflict, not a write: {error}"
            );
        }
        _ => panic!("a page edited after the context read must conflict"),
    }
    assert_eq!(
        latest(&f).await,
        Some(edited),
        "B is still the latest version"
    );
    let md = f.wiki.read_page(f.ws, f.proj, &anchor(&f)).unwrap();
    assert!(md.body.contains("concurrent edit B"));
    assert!(!md.body.contains("Derived from A."));
    assert!(
        recorded_effects(&f, &run).await.is_empty(),
        "nothing was published or checkpointed"
    );
}

/// Revokes the viewer's grant while its first call is in flight.
struct RevokingLlm {
    inner: StageLlm,
    writer: WriterHandle,
    user: ai_memory_core::UserId,
    proj: ProjectId,
}

#[async_trait::async_trait]
impl LlmProvider for RevokingLlm {
    fn name(&self) -> &'static str {
        "revoking-llm"
    }
    fn model(&self) -> &str {
        "revoking-llm"
    }
    async fn complete(&self, r: ChatRequest) -> LlmResult<ai_memory_llm::ChatResponse> {
        self.inner.complete(r).await
    }
    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: Value,
    ) -> LlmResult<Value> {
        let first = self.inner.count.load(Ordering::SeqCst) == 0;
        let out = self.inner.complete_structured_raw(request, schema).await;
        if first {
            self.writer
                .revoke_memory(self.user, self.proj, None)
                .await
                .unwrap();
        }
        out
    }
}

#[tokio::test]
async fn a_grant_revoked_between_calls_stops_the_next_dispatch_and_publishes_nothing() {
    let f = fixture(20, 3_000).await;
    let user = member(&f, ai_memory_store::GrantLevel::Write).await;
    let llm = Arc::new(RevokingLlm {
        inner: StageLlm::new(),
        writer: f.store.writer.clone(),
        user,
        proj: f.proj,
    });
    let calls = llm.inner.calls.clone();
    let svc = service(&f, llm, true, 128);
    svc.start(viewed_by(&f, user)).await.unwrap();
    let status = svc.execute_next().await.unwrap().unwrap();
    assert_eq!(status.state, ProcessingJobState::Invalidated);
    assert_eq!(
        calls.lock().unwrap().len(),
        1,
        "the call in flight finished; no later request was sent"
    );
    assert!(
        f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_err(),
        "nothing published"
    );
    assert!(svc.execute_next().await.unwrap().is_none());
    assert_eq!(calls.lock().unwrap().len(), 1);
}

/// The hooks create `sessions/<id>.md` before consolidation runs, so a batch
/// that rewrites it replaces an existing page: its id and body belong to the
/// final call's context, never to a later read.
#[tokio::test]
async fn multi_context_captures_the_existing_session_anchor_id_and_body() {
    let f = fixture(6, 500).await;
    let hook_page = write_anchor(&f, "hook session stub body").await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    svc.start(start(&f, ProcessingJobKind::ConsolidateMulti))
        .await
        .unwrap();
    let run = claimed_run(&f, &svc).await;
    let ctx = match run.consolidate_context(true).await {
        Ok(ctx) => ctx,
        Err(_) => panic!("multi context"),
    };
    assert_eq!(ctx.bases[anchor(&f).as_str()], Some(hook_page.to_string()));
    assert!(ctx.blocks.contains("hook session stub body"));
}

#[tokio::test]
async fn multi_page_publication_replaces_an_existing_session_anchor() {
    let f = fixture(12, 2_500).await;
    let hook_page = write_anchor(&f, "hook session stub body").await;
    let svc = service(&f, Arc::new(StageLlm::new()), true, 128);
    let outcomes = svc
        .consolidate_session_multi(f.session, false, ProcessingCaller::default(), None)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 2);
    assert!(outcomes.iter().all(|o| o.page_id.is_some()));
    assert_ne!(
        latest(&f).await,
        Some(hook_page),
        "the anchor got a new version"
    );
    let page = f.wiki.read_page(f.ws, f.proj, &anchor(&f)).unwrap();
    assert!(!page.body.contains("hook session stub body"));
    assert!(page.frontmatter.get("processing_job").is_some());
}

/// A provider whose every call makes `sends` real generation sends, each
/// announced through the attempt hook exactly like a terminal provider send
/// (a strict→tolerant re-send is two). `revoke` pulls the viewer's grant
/// between the first and second send.
struct SendingLlm {
    inner: StageLlm,
    sends: usize,
    /// Sends announced here; the last send of every call is announced by
    /// `inner`, like the single send of any terminal provider.
    extra: AtomicUsize,
    revoke: Option<(WriterHandle, ai_memory_core::UserId, ProjectId)>,
}

impl SendingLlm {
    fn new(sends: usize) -> Self {
        Self {
            inner: StageLlm::new(),
            sends,
            extra: AtomicUsize::new(0),
            revoke: None,
        }
    }

    /// Sends the guard let through.
    fn granted(&self) -> usize {
        self.extra.load(Ordering::SeqCst) + self.inner.count.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl LlmProvider for SendingLlm {
    fn name(&self) -> &'static str {
        "sending-llm"
    }
    fn model(&self) -> &str {
        "sending-llm"
    }
    async fn complete(&self, r: ChatRequest) -> LlmResult<ai_memory_llm::ChatResponse> {
        self.inner.complete(r).await
    }
    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: Value,
    ) -> LlmResult<Value> {
        for _ in 1..self.sends {
            before_generation_send().await?;
            self.extra.fetch_add(1, Ordering::SeqCst);
        }
        if self.sends > 1
            && let Some((writer, user, proj)) = &self.revoke
        {
            writer.revoke_memory(*user, *proj, None).await.unwrap();
        }
        self.inner.complete_structured_raw(request, schema).await
    }
}

/// A leaf that sends once, then answers with a transient server error.
struct DownLeaf {
    granted: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for DownLeaf {
    fn name(&self) -> &'static str {
        "down-leaf"
    }
    fn model(&self) -> &str {
        "down-leaf"
    }
    async fn complete(&self, _r: ChatRequest) -> LlmResult<ai_memory_llm::ChatResponse> {
        unreachable!("chunked processing only uses structured completion")
    }
    async fn complete_structured_raw(
        &self,
        _request: ChatRequest,
        _schema: Value,
    ) -> LlmResult<Value> {
        before_generation_send().await?;
        self.granted.fetch_add(1, Ordering::SeqCst);
        Err(LlmError::Provider {
            status: 503,
            body: "down".into(),
            retry_after: None,
        })
    }
}

async fn run_job(
    f: &Fixture,
    llm: Arc<dyn LlmProvider>,
    max_calls: u32,
    start: ProcessingJobStart,
) -> ai_memory_core::ProcessingJobStatus {
    let svc = service(f, llm, true, max_calls);
    svc.start(start).await.unwrap();
    svc.execute_next().await.unwrap().unwrap()
}

#[tokio::test]
async fn every_actual_send_is_durably_spent_and_a_refused_extra_send_publishes_nothing() {
    // Control: a budget that covers both sends of every call completes, and
    // the durable counter equals the sends that actually left.
    let f = fixture(12, 2_500).await;
    let llm = Arc::new(SendingLlm::new(2));
    let job = start(&f, ProcessingJobKind::Consolidate);
    let status = run_job(&f, llm.clone(), 128, job).await;
    assert_eq!(status.state, ProcessingJobState::Completed);
    let logical = llm.inner.count.load(Ordering::SeqCst);
    assert!(
        logical >= 2,
        "the session needs several calls, got {logical}"
    );
    assert_eq!(llm.granted(), 2 * logical);
    assert_eq!(status.calls_used as usize, 2 * logical);
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_ok());

    // One send short: the last call's second send is refused by the guard,
    // the job ends BudgetExceeded with every granted send counted and
    // nothing published.
    let g = fixture(12, 2_500).await;
    let tight = Arc::new(SendingLlm::new(2));
    let max = u32::try_from(2 * logical - 1).unwrap();
    let job = start(&g, ProcessingJobKind::Consolidate);
    let status = run_job(&g, tight.clone(), max, job).await;
    assert_eq!(status.state, ProcessingJobState::BudgetExceeded);
    assert_eq!(status.calls_used, max);
    assert_eq!(tight.granted(), max as usize);
    assert_eq!(
        tight.inner.count.load(Ordering::SeqCst),
        logical - 1,
        "the refused call never reached the model"
    );
    assert!(g.wiki.read_page(g.ws, g.proj, &anchor(&g)).is_err());
}

#[tokio::test]
async fn a_fallback_candidate_send_is_spent_like_any_other() {
    let build = || {
        let down = Arc::new(DownLeaf {
            granted: AtomicUsize::new(0),
        });
        let up = Arc::new(SendingLlm::new(1));
        let chain = Arc::new(ai_memory_llm::FallbackLlmProvider::new(vec![
            ai_memory_llm::Candidate::new("down", "down-leaf", down.clone()),
            ai_memory_llm::Candidate::new("up", "sending-llm", up.clone()),
        ]));
        (down, up, chain)
    };
    // The primary is skipped once its circuit opens, so exactly one call
    // makes two real sends: the primary's and the secondary's.
    let f = fixture(12, 2_500).await;
    let (down, up, chain) = build();
    let status = run_job(&f, chain, 128, start(&f, ProcessingJobKind::Consolidate)).await;
    assert_eq!(status.state, ProcessingJobState::Completed);
    let logical = up.inner.count.load(Ordering::SeqCst);
    let sent = down.granted.load(Ordering::SeqCst) + up.granted();
    assert_eq!(down.granted.load(Ordering::SeqCst), 1);
    assert_eq!(sent, logical + 1);
    assert_eq!(status.calls_used as usize, sent);

    // A ceiling equal to the logical call count cannot also cover the hidden
    // fallback send (the control proved it was counted): the job stops at it.
    let g = fixture(12, 2_500).await;
    let (down, up, chain) = build();
    let max = u32::try_from(logical).unwrap();
    let status = run_job(&g, chain, max, start(&g, ProcessingJobKind::Consolidate)).await;
    assert_eq!(status.state, ProcessingJobState::BudgetExceeded);
    assert_eq!(status.calls_used, max);
    assert_eq!(
        down.granted.load(Ordering::SeqCst) + up.granted(),
        max as usize
    );
    assert!(g.wiki.read_page(g.ws, g.proj, &anchor(&g)).is_err());
}

#[tokio::test]
async fn a_grant_revoked_before_an_extra_send_refuses_it_and_invalidates_the_job() {
    let f = fixture(12, 2_500).await;
    let user = member(&f, ai_memory_store::GrantLevel::Write).await;
    let mut llm = SendingLlm::new(2);
    llm.revoke = Some((f.store.writer.clone(), user, f.proj));
    let llm = Arc::new(llm);
    let status = run_job(&f, llm.clone(), 128, viewed_by(&f, user)).await;
    assert_eq!(status.state, ProcessingJobState::Invalidated);
    assert_eq!(status.calls_used, 1, "only the first send was spent");
    assert_eq!(llm.granted(), 1);
    assert_eq!(llm.inner.count.load(Ordering::SeqCst), 0);
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_err());
}

/// Run a statement against the database file from outside the engine, the way
/// an operator tool or a crash window would.
fn sql(f: &Fixture, statement: &str) {
    let conn = rusqlite::Connection::open(f.store.db_path()).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    conn.execute_batch(statement).unwrap();
}

/// Let a job parked by a retry run again now instead of after its delay.
fn make_due(f: &Fixture) {
    sql(f, "UPDATE processing_jobs SET next_run_at = 0");
}

/// Refuses every call the way a full or timed-out admission queue does:
/// before the provider is touched, so no send is ever announced.
struct SaturatedLlm {
    inner: StageLlm,
    saturated: AtomicBool,
    refused: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for SaturatedLlm {
    fn name(&self) -> &'static str {
        "saturated-llm"
    }
    fn model(&self) -> &str {
        "saturated-llm"
    }
    async fn complete(&self, r: ChatRequest) -> LlmResult<ai_memory_llm::ChatResponse> {
        self.inner.complete(r).await
    }
    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: Value,
    ) -> LlmResult<Value> {
        if self.saturated.load(Ordering::SeqCst) {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return Err(LlmError::CapacityBusy("llm admission"));
        }
        self.inner.complete_structured_raw(request, schema).await
    }
}

/// Answers a transient 503 to three of every four attempts, each of them a
/// real, announced send.
struct FlakyLlm {
    inner: StageLlm,
    attempts: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for FlakyLlm {
    fn name(&self) -> &'static str {
        "flaky-llm"
    }
    fn model(&self) -> &str {
        "flaky-llm"
    }
    async fn complete(&self, r: ChatRequest) -> LlmResult<ai_memory_llm::ChatResponse> {
        self.inner.complete(r).await
    }
    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: Value,
    ) -> LlmResult<Value> {
        if !self
            .attempts
            .fetch_add(1, Ordering::SeqCst)
            .is_multiple_of(4)
        {
            before_generation_send().await?;
            return Err(LlmError::Provider {
                status: 503,
                body: "down".into(),
                retry_after: None,
            });
        }
        self.inner.complete_structured_raw(request, schema).await
    }
}

#[tokio::test]
async fn a_saturated_queue_makes_no_provider_traffic_and_spends_no_call() {
    let f = fixture(12, 2_500).await;
    let llm = Arc::new(SaturatedLlm {
        inner: StageLlm::new(),
        saturated: AtomicBool::new(true),
        refused: AtomicUsize::new(0),
    });
    let svc = service(&f, llm.clone(), true, 128);
    svc.start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();

    let parked = svc.execute_next().await.unwrap().unwrap();
    assert_eq!(parked.state, ProcessingJobState::Running, "rescheduled");
    assert!(llm.refused.load(Ordering::SeqCst) >= 1);
    assert_eq!(
        llm.inner.count.load(Ordering::SeqCst),
        0,
        "no provider traffic"
    );
    assert_eq!(parked.calls_used, 0, "no call was spent without a send");

    // Control: once the queue drains the same job completes within budget.
    llm.saturated.store(false, Ordering::SeqCst);
    make_due(&f);
    let done = svc.execute_next().await.unwrap().unwrap();
    assert_eq!(done.state, ProcessingJobState::Completed);
    assert_eq!(
        done.calls_used as usize,
        llm.inner.count.load(Ordering::SeqCst),
        "every spent call is a real send"
    );
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_ok());
}

#[tokio::test]
async fn a_provider_that_stays_down_ends_the_job_after_the_retry_limit() {
    let f = fixture(12, 2_500).await;
    let llm = Arc::new(DownLeaf {
        granted: AtomicUsize::new(0),
    });
    // Far more budget than the limit can use: the claims, not the budget, stop it.
    let svc = service(&f, llm.clone(), true, 512);
    svc.start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();
    let mut claims = 0u32;
    let status = loop {
        let status = svc.execute_next().await.unwrap().expect("a due job");
        claims += 1;
        if status.state == ProcessingJobState::Failed {
            break status;
        }
        assert!(claims < 3 * MAX_ERROR_RETRIES, "the retries are unbounded");
        make_due(&f);
    };
    assert_eq!(claims, MAX_ERROR_RETRIES);
    // Three attempts per claim, every one a real send.
    assert_eq!(status.calls_used, 3 * MAX_ERROR_RETRIES);
    assert_eq!(
        llm.granted.load(Ordering::SeqCst),
        (3 * MAX_ERROR_RETRIES) as usize
    );
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_err());
}

#[tokio::test]
async fn progress_forgets_earlier_transient_failures() {
    // Every claim makes one step of progress and then hits an outage: more
    // claims than the retry limit end on a failure, none twice in a row.
    let f = fixture(40, 3_000).await;
    let llm = Arc::new(FlakyLlm {
        inner: StageLlm::new(),
        attempts: AtomicUsize::new(0),
    });
    let svc = service(&f, llm.clone(), true, 1024);
    svc.start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();
    let mut claims = 0u32;
    let status = loop {
        let status = svc.execute_next().await.unwrap().expect("a due job");
        claims += 1;
        if status.state == ProcessingJobState::Completed
            || status.state == ProcessingJobState::Failed
        {
            break status;
        }
        assert!(claims < 400, "the job makes no progress");
        make_due(&f);
    };
    assert_eq!(status.state, ProcessingJobState::Completed);
    assert!(
        claims > MAX_ERROR_RETRIES,
        "only {claims} claims: the limit must count consecutive failures"
    );
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_ok());
}

#[tokio::test]
async fn a_crash_after_the_last_call_resumes_from_the_stored_final_with_no_new_request() {
    // Control: the whole session in as many calls as it needs.
    let probe = fixture(12, 2_500).await;
    let done = run_job(
        &probe,
        Arc::new(StageLlm::new()),
        128,
        start(&probe, ProcessingJobKind::Consolidate),
    )
    .await;
    assert_eq!(done.state, ProcessingJobState::Completed);
    let total = done.calls_used;

    // A budget of exactly that many calls; the process dies after the final
    // answer is stored and before the publication plan is.
    let f = fixture(12, 2_500).await;
    let llm = Arc::new(StageLlm::new());
    let svc = service(&f, llm.clone(), true, total);
    svc.start(start(&f, ProcessingJobKind::Consolidate))
        .await
        .unwrap();
    sql(
        &f,
        "CREATE TRIGGER crash_before_plan BEFORE UPDATE OF publication_identity \
         ON processing_jobs BEGIN SELECT RAISE(ABORT, 'simulated crash'); END",
    );
    assert!(svc.execute_next().await.is_err());
    assert_eq!(llm.count.load(Ordering::SeqCst), total as usize);
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_err());

    sql(&f, "DROP TRIGGER crash_before_plan");
    make_due(&f);
    let resumed = svc.execute_next().await.unwrap().unwrap();
    assert_eq!(resumed.state, ProcessingJobState::Completed);
    assert_eq!(resumed.calls_used, total, "no call beyond the budget");
    assert_eq!(
        llm.count.load(Ordering::SeqCst),
        total as usize,
        "the stored final answer was reused, nothing was requested"
    );
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_ok());
}

#[tokio::test]
async fn a_truncated_final_on_a_single_digest_is_never_resent_unchanged() {
    let f = fixture(4, 300).await;
    let mut llm = StageLlm::new();
    llm.truncate_final = true;
    let llm = Arc::new(llm);
    let calls = llm.calls.clone();
    let status = run_job(&f, llm, 128, start(&f, ProcessingJobKind::Consolidate)).await;
    assert_eq!(status.state, ProcessingJobState::Failed);
    let finals = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| !c.stage.ends_with("MAP") && !c.stage.ends_with("REDUCE"))
        .count();
    assert_eq!(
        finals, 1,
        "the only final request failed and nothing smaller exists to send"
    );
    assert!(f.wiki.read_page(f.ws, f.proj, &anchor(&f)).is_err());
}

/// Enqueue and run one auto-improve job whose every final answer is `answer`.
async fn review_job_answering(answer: Value) -> (Fixture, ai_memory_core::ProcessingJobStatus) {
    let f = fixture(4, 300).await;
    let mut llm = StageLlm::new();
    llm.malformed_final = Some(answer);
    let svc = service(&f, Arc::new(llm), true, 128);
    svc.enqueue(
        &start(&f, ProcessingJobKind::AutoImprove),
        &super::StartOptions {
            instructions: None,
            review: Some(AutoImproveReviewConfig {
                min_observations: 1,
                min_session_duration_secs: 0,
                ..AutoImproveReviewConfig::default()
            }),
            trigger: "durable",
        },
    )
    .await
    .unwrap();
    let status = svc.execute_next().await.unwrap().unwrap();
    (f, status)
}

#[tokio::test]
async fn a_malformed_final_review_never_completes_as_an_empty_staged_report() {
    for wrong in [
        json!({}),
        json!({ "facts": [{ "text": "a mapped candidate" }] }),
        json!({ "summary": "s", "proposals": [] }),
        json!({ "summary": "s", "proposals": [], "rejected_candidates": [], "facts": [] }),
    ] {
        let (f, status) = review_job_answering(wrong.clone()).await;
        assert_eq!(
            status.state,
            ProcessingJobState::Failed,
            "{wrong} must not read as a finished review"
        );
        assert_eq!(staged_rows(&f), (0, 0), "{wrong} staged nothing");
    }
}

#[tokio::test]
async fn an_explicitly_empty_final_review_still_completes() {
    let (_f, status) = review_job_answering(json!({
        "summary": "nothing durable",
        "proposals": [],
        "rejected_candidates": []
    }))
    .await;
    assert_eq!(status.state, ProcessingJobState::Completed);
}

mod focus_base;
