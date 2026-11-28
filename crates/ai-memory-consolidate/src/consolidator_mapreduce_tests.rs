//! Regressions for the opt-in map-reduce consolidation path. This module is
//! included from `consolidator.rs` so it can exercise the private planners and
//! validators through the same public entries the server uses.

use super::*;
use crate::types::{ExtractionKind, ExtractionResult};
use ai_memory_core::{ObservationId, ObservationKind};
use jiff::Timestamp;
use regex::Regex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn chunk_observation(body: impl Into<String>) -> Observation {
    Observation {
        trust_class: ai_memory_core::TrustClass::ModelGenerated,
        id: ObservationId::new(),
        workspace_id: WorkspaceId::new(),
        project_id: ProjectId::new(),
        session_id: SessionId::new(),
        kind: ObservationKind::UserPrompt,
        title: "t".into(),
        body: body.into(),
        created_at: Timestamp::UNIX_EPOCH,
        importance: 5,
        extension: None,
        source_event: None,
    }
}

fn part_body(part: &ChunkPart) -> &str {
    part.text
        .split_once("body:\n")
        .expect("every part has a body header")
        .1
        .strip_suffix('\n')
        .expect("every part ends with a newline")
}

#[test]
fn map_parts_do_not_drop_characters_or_ids_including_empty_bodies() {
    let empty = chunk_observation("");
    let big = chunk_observation("word ".repeat(4_000));
    let parts = plan_map_parts(&[empty.clone(), big.clone()]);
    let empty_parts: Vec<_> = parts
        .iter()
        .filter(|p| p.observation_id == empty.id.to_string())
        .collect();
    assert_eq!(empty_parts.len(), 1, "an empty body still names its id");
    assert_eq!(part_body(empty_parts[0]), "");
    let big_parts: Vec<_> = parts
        .iter()
        .filter(|p| p.observation_id == big.id.to_string())
        .collect();
    assert!(big_parts.len() > 1, "an oversized body must split");
    assert!(
        big_parts
            .iter()
            .all(|p| p.part_total as usize == big_parts.len())
    );
    assert!(
        big_parts
            .iter()
            .enumerate()
            .all(|(i, p)| p.part_index as usize == i + 1)
    );
    let rebuilt: String = big_parts.iter().map(|p| part_body(p)).collect();
    assert_eq!(rebuilt, big.body, "no character may be lost at a cut");
}

#[test]
fn map_block_fingerprint_is_content_derived() {
    let obs = chunk_observation("stable body");
    let cfg = |model: &str| ChunkingConfig {
        target_chars: 5_000,
        model: model.into(),
    };
    let a = plan_map_blocks(
        SessionId::new(),
        std::slice::from_ref(&obs),
        &cfg("m1"),
        20_000,
    )
    .unwrap();
    let b = plan_map_blocks(
        SessionId::new(),
        std::slice::from_ref(&obs),
        &cfg("m1"),
        20_000,
    )
    .unwrap();
    assert_eq!(a[0].fingerprint, b[0].fingerprint);
    let mut changed = obs.clone();
    changed.body.push('x');
    let c = plan_map_blocks(SessionId::new(), &[changed], &cfg("m1"), 20_000).unwrap();
    assert_ne!(a[0].fingerprint, c[0].fingerprint);
    let d = plan_map_blocks(SessionId::new(), &[obs], &cfg("m2"), 20_000).unwrap();
    assert_ne!(a[0].fingerprint, d[0].fingerprint);
}

#[test]
fn publication_marker_length_prefixes_prevent_hostile_alias() {
    let one = chunk_observation("abc");
    let two = chunk_observation("def");
    let forged = chunk_observation("3:def|3:abc|");
    let a = publication_marker("single", "m1", "", &[one.clone(), two.clone()]);
    let b = publication_marker("single", "m1", "", &[one.clone(), forged]);
    assert_ne!(a, b);
    assert_ne!(a, publication_marker("single", "m2", "", &[one, two]));
}

#[test]
fn extraction_validation_rejects_ungrounded_and_incomplete_stages() {
    let allowed: HashSet<String> = ["id-1".to_owned(), "id-2".to_owned()].into();
    let extraction = |ids: &[&str], confidence| EvidenceExtraction {
        observation_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        kind: ExtractionKind::Fact,
        title: "t".into(),
        summary: "s".into(),
        body_markdown: "b".into(),
        tags: Vec::new(),
        entities: Vec::new(),
        confidence,
    };
    let result = |extractions: Vec<EvidenceExtraction>, no_durable: &[&str]| ExtractionResult {
        extractions,
        rationale: String::new(),
        no_durable_fact_ids: no_durable.iter().map(|id| (*id).to_owned()).collect(),
    };

    assert!(matches!(
        validate_extraction_result(&result(vec![extraction(&["id-3"], 0.9)], &[]), &allowed),
        Err(ConsolidatorError::UngroundedExtractions(_))
    ));
    assert!(matches!(
        validate_extraction_result(
            &result(vec![extraction(&["id-1"], 1.8)], &["id-2"]),
            &allowed
        ),
        Err(ConsolidatorError::UngroundedExtractions(_))
    ));
    assert!(matches!(
        validate_extraction_result(&result(Vec::new(), &["id-1"]), &allowed),
        Err(ConsolidatorError::IncompleteCoverage(_))
    ));
    validate_extraction_result(
        &result(vec![extraction(&["id-1"], 0.7)], &["id-2"]),
        &allowed,
    )
    .expect("a fully accounted stage is the legitimate control");
    assert!(
        redacted_error_summary(&ConsolidatorError::IncompleteCoverage(
            "id-1 private detail".into()
        ))
        .contains("class=incomplete-coverage status=none")
    );
}

async fn chunked_fixture(
    tmp: &std::path::Path,
    body_sizes: &[usize],
) -> (
    ai_memory_store::Store,
    Wiki,
    SessionId,
    WorkspaceId,
    ProjectId,
) {
    let store = ai_memory_store::Store::open(tmp).unwrap();
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
    store
        .writer
        .begin_session(ai_memory_core::NewSession {
            occurred_at: None,
            id: session,
            workspace_id: ws,
            project_id: proj,
            agent_kind: ai_memory_core::AgentKind::Codex,
            cwd: None,
            actor_user: None,
            origin: None,
        })
        .await
        .unwrap();
    for (i, size) in body_sizes.iter().enumerate() {
        store
            .writer
            .insert_observation(ai_memory_core::Sanitized::new(
                ai_memory_core::NewObservation {
                    occurred_at: None,
                    session_id: session,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: format!("obs {i}"),
                    body: if i == 0 {
                        "SENTINEL_PRIVATE_BODY ".repeat(*size)
                    } else {
                        format!("word-{i} ").repeat(*size)
                    },
                    importance: 5,
                    file_path: None,
                    trust_class: None,
                },
                &ai_memory_core::Sanitizer::default(),
            ))
            .await
            .unwrap();
    }
    let wiki = Wiki::new(tmp, store.writer.clone()).unwrap();
    (store, wiki, session, ws, proj)
}

fn observation_ids_in_map_request(user: &str) -> Vec<String> {
    let re = Regex::new(r"--- observation ([0-9a-fA-F-]{36})").unwrap();
    let mut ids: Vec<String> = re.captures_iter(user).map(|c| c[1].to_owned()).collect();
    ids.dedup();
    ids
}

fn observation_ids_in_reduce_request(user: &str) -> Vec<String> {
    let re = Regex::new(r"observation_ids: ([0-9a-fA-F, -]+)").unwrap();
    let mut ids = Vec::new();
    for cap in re.captures_iter(user) {
        for piece in cap[1].split(',') {
            let id = piece.trim();
            if !id.is_empty() {
                ids.push(id.to_owned());
            }
        }
    }
    ids.dedup();
    ids
}

fn extraction_reply(ids: Vec<String>) -> serde_json::Value {
    serde_json::json!({
        "extractions": [{
            "observation_ids": ids,
            "kind": "fact",
            "title": "Grounded evidence",
            "summary": "A grounded claim.",
            "body_markdown": "A short grounded note.",
            "confidence": 0.9,
        }],
        "rationale": "",
        "no_durable_fact_ids": [],
    })
}

/// Deterministic provider for the three stages. Map/reduce replies cite
/// exactly the ids the request showed it; final replies a fixed page. The
/// switches inject a crash before publication or an ungrounded map output.
struct StagedLlm {
    map_calls: AtomicUsize,
    reduce_calls: AtomicUsize,
    final_calls: AtomicUsize,
    fail_final: AtomicBool,
    bad_map: AtomicBool,
    final_user: std::sync::Mutex<String>,
    agent_page_on_final: std::sync::Mutex<Option<(Wiki, WorkspaceId, ProjectId, PagePath)>>,
}

impl StagedLlm {
    fn new() -> Self {
        Self {
            map_calls: AtomicUsize::new(0),
            reduce_calls: AtomicUsize::new(0),
            final_calls: AtomicUsize::new(0),
            fail_final: AtomicBool::new(false),
            bad_map: AtomicBool::new(false),
            final_user: std::sync::Mutex::new(String::new()),
            agent_page_on_final: std::sync::Mutex::new(None),
        }
    }

    fn calls(&self) -> (usize, usize, usize) {
        (
            self.map_calls.load(Ordering::SeqCst),
            self.reduce_calls.load(Ordering::SeqCst),
            self.final_calls.load(Ordering::SeqCst),
        )
    }
}

#[async_trait::async_trait]
impl LlmProvider for StagedLlm {
    fn name(&self) -> &'static str {
        "staged"
    }
    fn model(&self) -> &str {
        "staged"
    }
    async fn complete(
        &self,
        _request: ChatRequest,
    ) -> ai_memory_llm::LlmResult<ai_memory_llm::ChatResponse> {
        unreachable!("consolidation uses structured output")
    }
    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        _schema: serde_json::Value,
    ) -> ai_memory_llm::LlmResult<serde_json::Value> {
        let user = request.messages[0].content.clone();
        if request
            .system
            .as_deref()
            .is_some_and(|s| s.contains("map stage"))
        {
            self.map_calls.fetch_add(1, Ordering::SeqCst);
            if self.bad_map.swap(false, Ordering::SeqCst) {
                return Ok(extraction_reply(vec!["not-an-observation".into()]));
            }
            return Ok(extraction_reply(observation_ids_in_map_request(&user)));
        }
        if request
            .system
            .as_deref()
            .is_some_and(|s| s.contains("reduce stage"))
        {
            self.reduce_calls.fetch_add(1, Ordering::SeqCst);
            return Ok(extraction_reply(observation_ids_in_reduce_request(&user)));
        }
        self.final_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_final.swap(false, Ordering::SeqCst) {
            return Err(LlmError::Auth("crash after map checkpoints".into()));
        }
        // Simulate an agent writing its own page after the worker's first
        // check but before its conditional write.
        let agent_write = self.agent_page_on_final.lock().unwrap().take();
        if let Some((wiki, ws, proj, path)) = agent_write {
            wiki.write_page(WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path,
                frontmatter: serde_json::json!({
                    "title": "Agent page",
                    "consolidated_by": "agent",
                }),
                body: "Written by the agent".into(),
                tier: Tier::Episodic,
                pinned: false,
                title: None,
                admission_ctx: None,
                author_id: None,
                actor: ai_memory_core::ActorContext::anonymous(),
                evidence: Vec::new(),
                trust_class: None,
            })
            .await
            .expect("the simulated agent write must land");
        }
        *self.final_user.lock().unwrap() = user;
        Ok(serde_json::json!({
            "title": "Map-reduce page",
            "body_markdown": "Grounded by the evidence section.",
            "tags": [],
        }))
    }
}

fn staged_consolidator(
    store: &ai_memory_store::Store,
    wiki: Wiki,
    llm: Arc<StagedLlm>,
    ws: WorkspaceId,
    proj: ProjectId,
    chunk_tokens: usize,
) -> Consolidator {
    Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki,
        llm,
        ws,
        proj,
    )
    .with_prompt_limits(9_000, 32_000, 0.8)
    .with_chunking(chunk_tokens, 0.8)
}

#[tokio::test]
async fn chunked_end_to_end_writes_page_with_all_sources_and_prunes_checkpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900, 900]).await;
    let llm = Arc::new(StagedLlm::new());
    let outcome = staged_consolidator(&store, wiki.clone(), llm.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("an oversized session must consolidate through map-reduce");
    let calls = llm.calls();
    assert!(calls.0 >= 1 && calls.2 == 1, "got {calls:?}");
    let page = wiki.read_page(ws, proj, &outcome.path).unwrap();
    assert!(page.frontmatter.get(CONSOLIDATION_MARKER_KEY).is_some());
    assert!(
        !llm.final_user
            .lock()
            .unwrap()
            .contains("SENTINEL_PRIVATE_BODY")
    );
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let sources: usize = conn
        .query_row(
            "SELECT COUNT(*) FROM page_sources WHERE page_id = ?1",
            [outcome.page_id.unwrap().as_bytes()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sources, 3, "every observation id must be grounded");
    assert!(
        store
            .writer
            .load_consolidation_chunks(ws, proj, session)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn chunked_crash_between_checkpoint_and_publish_reuses_map() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900, 900]).await;
    let first = Arc::new(StagedLlm::new());
    first.fail_final.store(true, Ordering::SeqCst);
    staged_consolidator(&store, wiki.clone(), first.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect_err("the interrupted run must fail");
    assert!(
        !store
            .writer
            .load_consolidation_chunks(ws, proj, session)
            .await
            .unwrap()
            .is_empty()
    );
    let second = Arc::new(StagedLlm::new());
    staged_consolidator(&store, wiki.clone(), second.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("a fresh process must resume from checkpoints");
    assert_eq!(
        second.calls().0,
        0,
        "finished map blocks must not call the LLM again"
    );
    assert_eq!(second.calls().2, 1);
}

#[tokio::test]
async fn chunked_crash_after_publish_before_prune_reconciles_without_llm() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900, 900]).await;
    let first = Arc::new(StagedLlm::new());
    first.fail_final.store(true, Ordering::SeqCst);
    staged_consolidator(&store, wiki.clone(), first.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect_err("the interrupted run must fail");
    let observations = store
        .reader
        .observations_for_session_in_scope(ws, proj, session)
        .await
        .unwrap();
    let marker = publication_marker("single", "staged", "", &observations);
    let path = PagePath::new(format!("sessions/{session}.md")).unwrap();
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: path.clone(),
        frontmatter: serde_json::json!({
            "title": "Already published",
            CONSOLIDATION_MARKER_KEY: marker,
        }),
        body: "Published body".into(),
        tier: Tier::Episodic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
        trust_class: None,
    })
    .await
    .unwrap();
    let second = Arc::new(StagedLlm::new());
    let outcome = staged_consolidator(&store, wiki, second.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("the published page reconciles without another call");
    assert_eq!(second.calls(), (0, 0, 0));
    assert_eq!(outcome.new_title, "Already published");
    assert!(
        store
            .writer
            .load_consolidation_chunks(ws, proj, session)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn chunked_agent_page_is_not_reconciled_or_overwritten() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900, 900]).await;
    let first = Arc::new(StagedLlm::new());
    first.fail_final.store(true, Ordering::SeqCst);
    staged_consolidator(&store, wiki.clone(), first.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect_err("the interrupted run must fail");
    let observations = store
        .reader
        .observations_for_session_in_scope(ws, proj, session)
        .await
        .unwrap();
    let marker = publication_marker("single", "staged", "", &observations);
    let path = PagePath::new(format!("sessions/{session}.md")).unwrap();
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: path.clone(),
        frontmatter: serde_json::json!({
            "title": "Agent page",
            "consolidated_by": "agent",
            CONSOLIDATION_MARKER_KEY: marker,
        }),
        body: "Written by the agent".into(),
        tier: Tier::Episodic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
        trust_class: None,
    })
    .await
    .unwrap();
    let second = Arc::new(StagedLlm::new());
    let result = staged_consolidator(&store, wiki.clone(), second.clone(), ws, proj, 6_000)
        .consolidate_session_keeping_agent_page(session, ai_memory_core::ActorContext::anonymous())
        .await
        .expect("an agent page ends the job cleanly");
    assert!(matches!(result, SessionPageConsolidation::AgentPageKept));
    assert_eq!(second.calls(), (0, 0, 0));
    assert_eq!(
        wiki.read_page(ws, proj, &path).unwrap().body,
        "Written by the agent"
    );
    assert!(
        store
            .writer
            .load_consolidation_chunks(ws, proj, session)
            .await
            .unwrap()
            .is_empty(),
        "the agent page wins, but its checkpoints must still be pruned"
    );
    // The manual memory_consolidate entry has no worker-side early exit:
    // the same agent page must also survive that path without a call.
    // Seed a fresh checkpoint so this path proves its own prune instead of
    // inheriting the worker's.
    store
        .writer
        .record_consolidation_chunk(
            ws,
            proj,
            session,
            "agent-reconcile-probe".into(),
            "{}".into(),
        )
        .await
        .unwrap();
    assert!(
        !store
            .writer
            .load_consolidation_chunks(ws, proj, session)
            .await
            .unwrap()
            .is_empty(),
        "control: the seeded checkpoint is present before the manual run"
    );
    let third = Arc::new(StagedLlm::new());
    let outcome = staged_consolidator(&store, wiki.clone(), third.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("the manual entry must keep the agent page too");
    assert_eq!(outcome.new_title, "Agent page");
    assert_eq!(third.calls(), (0, 0, 0));
    assert_eq!(
        wiki.read_page(ws, proj, &path).unwrap().body,
        "Written by the agent"
    );
    assert!(
        store
            .writer
            .load_consolidation_chunks(ws, proj, session)
            .await
            .unwrap()
            .is_empty(),
        "the reconcile agent branch must prune too"
    );
}

#[tokio::test]
async fn chunked_agent_page_landing_during_the_run_prunes_checkpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900, 900]).await;
    let path = PagePath::new(format!("sessions/{session}.md")).unwrap();
    let llm = Arc::new(StagedLlm::new());
    *llm.agent_page_on_final.lock().unwrap() = Some((wiki.clone(), ws, proj, path.clone()));
    let result = staged_consolidator(&store, wiki.clone(), llm.clone(), ws, proj, 6_000)
        .consolidate_session_keeping_agent_page(session, ai_memory_core::ActorContext::anonymous())
        .await
        .expect("an agent page landing mid-run ends the job cleanly");
    assert!(matches!(result, SessionPageConsolidation::AgentPageKept));
    assert_eq!(
        wiki.read_page(ws, proj, &path).unwrap().body,
        "Written by the agent"
    );
    assert!(
        llm.calls().0 > 0,
        "control: map stages ran and recorded checkpoints before the write mismatch"
    );
    assert!(
        store
            .writer
            .load_consolidation_chunks(ws, proj, session)
            .await
            .unwrap()
            .is_empty(),
        "the mismatch agent branch must prune too"
    );
}

#[tokio::test]
async fn chunked_validation_failure_is_returned_on_the_first_call() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900]).await;
    let llm = Arc::new(StagedLlm::new());
    llm.bad_map.store(true, Ordering::SeqCst);
    let error = staged_consolidator(&store, wiki, llm.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect_err("an ungrounded extraction must fail closed");
    assert!(matches!(error, ConsolidatorError::UngroundedExtractions(_)));
    assert_eq!(llm.calls().0, 1, "validation fails before any retry");
    assert_eq!(llm.calls().2, 0);
    assert_eq!(
        redacted_error_summary(&error),
        "consolidation failed: class=ungrounded-extractions status=none"
    );
}

#[tokio::test]
async fn chunked_reduce_merges_until_the_final_request_fits() {
    let tmp = tempfile::tempdir().unwrap();
    let sizes: Vec<usize> = (0..200).map(|_| 60).collect();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &sizes).await;
    let llm = Arc::new(StagedLlm::new());
    staged_consolidator(&store, wiki.clone(), llm.clone(), ws, proj, 1_400)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("hierarchical reduce must make the final request fit");
    let (maps, reduces, finals) = llm.calls();
    assert!(
        maps > 1 && reduces >= 1 && finals == 1,
        "got {:?}",
        (maps, reduces, finals)
    );
    let total: usize = store
        .reader
        .observations_for_session_in_scope(ws, proj, session)
        .await
        .unwrap()
        .len();
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let sources: usize = conn
        .query_row("SELECT COUNT(*) FROM page_sources", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sources, total, "reduce must preserve every id");
}

/// Compare the captured request's fields without requiring `PartialEq` on
/// the provider-neutral type.
struct RequestCapturingLlm(std::sync::Mutex<Vec<ChatRequest>>);

#[async_trait::async_trait]
impl LlmProvider for RequestCapturingLlm {
    fn name(&self) -> &'static str {
        "capturing"
    }
    fn model(&self) -> &str {
        "capturing"
    }
    async fn complete(
        &self,
        _request: ChatRequest,
    ) -> ai_memory_llm::LlmResult<ai_memory_llm::ChatResponse> {
        unreachable!("consolidation uses structured output")
    }
    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        _schema: serde_json::Value,
    ) -> ai_memory_llm::LlmResult<serde_json::Value> {
        self.0.lock().unwrap().push(request);
        Ok(serde_json::json!({
            "title": "Untouched",
            "body_markdown": "same request",
            "tags": [],
        }))
    }
}

fn assert_same_request(expected: &ChatRequest, actual: &ChatRequest) {
    assert_eq!(expected.system, actual.system);
    assert_eq!(expected.max_tokens, actual.max_tokens);
    assert_eq!(expected.temperature, actual.temperature);
    assert_eq!(expected.messages.len(), actual.messages.len());
    for (a, b) in expected.messages.iter().zip(&actual.messages) {
        assert_eq!(a.role, b.role);
        assert_eq!(a.content, b.content);
    }
}

#[tokio::test]
async fn chunking_off_or_a_fitting_session_keeps_the_single_request_identical() {
    let budgets = PromptBudgets::from_limits_with_margin(100_000, 32_000, 0.8);
    for chunk_tokens in [0, 9_000] {
        let tmp = tempfile::tempdir().unwrap();
        let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[1]).await;
        let observations = store
            .reader
            .observations_for_session(session)
            .await
            .unwrap();
        let baseline = build_request(session, &observations, "", None, budgets, &[]);
        let llm = Arc::new(RequestCapturingLlm(std::sync::Mutex::new(Vec::new())));
        Consolidator::new(
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            llm.clone(),
            ws,
            proj,
        )
        .with_prompt_limits(100_000, 32_000, 0.8)
        .with_chunking(chunk_tokens, 0.8)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .unwrap();
        let requests = llm.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_same_request(&baseline, &requests[0]);
    }
}
