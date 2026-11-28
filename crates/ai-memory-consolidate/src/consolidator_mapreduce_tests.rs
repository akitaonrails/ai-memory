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
    assert_eq!(
        map_block_fingerprint(&cfg("m1"), &a[0].parts),
        map_block_fingerprint(&cfg("m1"), &b[0].parts)
    );
    let mut changed = obs.clone();
    changed.body.push('x');
    let c = plan_map_blocks(SessionId::new(), &[changed], &cfg("m1"), 20_000).unwrap();
    assert_ne!(
        map_block_fingerprint(&cfg("m1"), &a[0].parts),
        map_block_fingerprint(&cfg("m1"), &c[0].parts)
    );
    let d = plan_map_blocks(SessionId::new(), &[obs], &cfg("m2"), 20_000).unwrap();
    assert_ne!(
        map_block_fingerprint(&cfg("m1"), &a[0].parts),
        map_block_fingerprint(&cfg("m2"), &d[0].parts)
    );
}

#[test]
fn map_subblock_fingerprint_never_aliases_a_plain_block() {
    let cfg = ChunkingConfig {
        target_chars: 5_000,
        model: "m1".into(),
    };
    let obs: Vec<Observation> = (0..4)
        .map(|i| chunk_observation(format!("body {i}")))
        .collect();
    let parts = plan_map_parts(&obs);
    // The same parts as a plain block and as a split sub-block must not
    // collide, and neither must two different halves of the same split.
    let plain = map_block_fingerprint(&cfg, &parts);
    let whole_split = map_subblock_fingerprint(&cfg, &parts);
    assert_ne!(plain, whole_split);
    let left = map_subblock_fingerprint(&cfg, &parts[..2]);
    let right = map_subblock_fingerprint(&cfg, &parts[2..]);
    assert_ne!(left, right, "halves at different positions must differ");
    assert_ne!(left, whole_split);
    assert_ne!(left, map_block_fingerprint(&cfg, &parts[..2]));
    assert_eq!(left, map_subblock_fingerprint(&cfg, &parts[..2]));
    // A sub-block with parts in the other order is its own identity.
    let mut swapped = parts.clone();
    swapped[..2].reverse();
    assert_ne!(left, map_subblock_fingerprint(&cfg, &swapped[..2]));
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

/// A well-formed observation id that is never inserted by a fixture, so a
/// stage reply naming it cites an id outside its input.
const FOREIGN_OBSERVATION_ID: &str = "11111111-1111-4111-8111-111111111111";

/// The incomplete-coverage shape the production model returned: every id
/// but the last one accounted for. With a single id there is nothing left
/// to cite, so the shape is zero extractions and an empty
/// `no_durable_fact_ids` — the pure coverage hole, not a grounding break.
fn incomplete_coverage_reply(ids: Vec<String>) -> serde_json::Value {
    if ids.len() < 2 {
        return serde_json::json!({
            "extractions": [],
            "rationale": "",
            "no_durable_fact_ids": [],
        });
    }
    extraction_reply(ids[..ids.len() - 1].to_vec())
}

/// Deterministic provider for the three stages. Map/reduce replies cite
/// exactly the ids the request showed it; final replies a fixed page. The
/// switches inject a crash before publication or an ungrounded map output.
/// The coverage switches reply with one id LESS than the request showed,
/// which is the incomplete-coverage shape production saw from real models.
struct StagedLlm {
    map_calls: AtomicUsize,
    reduce_calls: AtomicUsize,
    final_calls: AtomicUsize,
    fail_final: AtomicBool,
    bad_map: AtomicBool,
    /// When non-zero and a map request shows at least this many distinct
    /// observation ids, the reply accounts for all but one id. Sticky.
    incomplete_when_ids_at_least: AtomicUsize,
    /// When non-zero, the Nth map call fails with an Auth error instead of
    /// replying (simulating a process crash mid split-block).
    fail_map_call: AtomicUsize,
    /// When non-zero, the Nth map call cites an id outside its stage input
    /// (a grounding break, which must stay terminal on every path).
    bad_map_call: AtomicUsize,
    /// The next map call replies with zero extractions and a
    /// `no_durable_fact_ids` that repeats an input id and names one id
    /// outside the stage input. Consumed once.
    weird_no_durable_map: AtomicBool,
    /// The next reduce call accounts for all but one of the ids its request
    /// cited. Consumed once.
    incomplete_reduce: AtomicBool,
    /// Observation ids per map call, in request order.
    map_request_ids: std::sync::Mutex<Vec<Vec<String>>>,
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
            incomplete_when_ids_at_least: AtomicUsize::new(0),
            fail_map_call: AtomicUsize::new(0),
            bad_map_call: AtomicUsize::new(0),
            weird_no_durable_map: AtomicBool::new(false),
            incomplete_reduce: AtomicBool::new(false),
            map_request_ids: std::sync::Mutex::new(Vec::new()),
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

    fn map_requests(&self) -> Vec<Vec<String>> {
        self.map_request_ids.lock().unwrap().clone()
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
            let call_no = self.map_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let ids = observation_ids_in_map_request(&user);
            self.map_request_ids.lock().unwrap().push(ids.clone());
            if self.bad_map.swap(false, Ordering::SeqCst) {
                return Ok(extraction_reply(vec!["not-an-observation".into()]));
            }
            let fail_at = self.fail_map_call.load(Ordering::SeqCst);
            if fail_at > 0 && call_no == fail_at {
                return Err(LlmError::Auth("crash mid split-block".into()));
            }
            let bad_at = self.bad_map_call.load(Ordering::SeqCst);
            if bad_at > 0 && call_no == bad_at {
                return Ok(extraction_reply(vec![FOREIGN_OBSERVATION_ID.into()]));
            }
            if self.weird_no_durable_map.swap(false, Ordering::SeqCst) {
                let mut no_durable: Vec<String> =
                    ids.iter().chain(ids.iter().rev()).cloned().collect();
                no_durable.push(FOREIGN_OBSERVATION_ID.into());
                return Ok(serde_json::json!({
                    "extractions": [],
                    "rationale": "",
                    "no_durable_fact_ids": no_durable,
                }));
            }
            let floor = self.incomplete_when_ids_at_least.load(Ordering::SeqCst);
            if floor > 0 && ids.len() >= floor {
                return Ok(incomplete_coverage_reply(ids));
            }
            return Ok(extraction_reply(ids));
        }
        if request
            .system
            .as_deref()
            .is_some_and(|s| s.contains("reduce stage"))
        {
            self.reduce_calls.fetch_add(1, Ordering::SeqCst);
            let ids = observation_ids_in_reduce_request(&user);
            if self.incomplete_reduce.swap(false, Ordering::SeqCst) {
                return Ok(incomplete_coverage_reply(ids));
            }
            return Ok(extraction_reply(ids));
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

/// Reads `page_sources` for one page.
fn page_source_count(store: &ai_memory_store::Store, page_id: ai_memory_core::PageId) -> usize {
    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM page_sources WHERE page_id = ?1",
        [page_id.as_bytes()],
        |row| row.get(0),
    )
    .unwrap()
}

#[tokio::test]
async fn chunked_incomplete_map_coverage_remaps_the_block_in_halves() {
    // Three oversized observations plan as blocks [p1], [p2, obs2], [obs3].
    // Every reply that saw two or more ids drops one, so the middle block
    // must be re-run as its two halves while the single-part blocks pass.
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900, 900]).await;
    let llm = Arc::new(StagedLlm::new());
    llm.incomplete_when_ids_at_least.store(2, Ordering::SeqCst);
    let outcome = staged_consolidator(&store, wiki.clone(), llm.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("incomplete map coverage must recover by re-mapping in halves, not fail");
    assert_eq!(
        llm.calls(),
        (5, 0, 1),
        "three block calls plus the two halves of the incomplete one"
    );
    let seq = llm.map_requests();
    let shapes: Vec<usize> = seq.iter().map(Vec::len).collect();
    assert_eq!(
        shapes,
        vec![1, 2, 1, 1, 1],
        "the incomplete 2-id block is followed by its halves, in order"
    );
    assert_eq!(seq[2], vec![seq[1][0].clone()], "the left half runs first");
    assert_eq!(
        seq[3],
        vec![seq[1][1].clone()],
        "the right half runs second"
    );
    let page = wiki
        .read_page(ws, proj, &outcome.path)
        .expect("the session page must still be written");
    assert!(page.frontmatter.get(CONSOLIDATION_MARKER_KEY).is_some());
    assert_eq!(
        page_source_count(&store, outcome.page_id.unwrap()),
        3,
        "page_sources must cover every id the (half) blocks cited"
    );
}

#[tokio::test]
async fn chunked_split_block_resumes_from_finished_subblock_checkpoints() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900, 900]).await;
    let first = Arc::new(StagedLlm::new());
    first
        .incomplete_when_ids_at_least
        .store(2, Ordering::SeqCst);
    first.fail_map_call.store(4, Ordering::SeqCst);
    let error = staged_consolidator(&store, wiki.clone(), first.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect_err("the run that crashes inside the split block must fail");
    assert!(matches!(error, ConsolidatorError::Llm(_)));
    assert_eq!(first.calls().0, 4);
    let first_seq = first.map_requests();
    assert_eq!(
        first_seq[2..4].to_vec(),
        vec![vec![first_seq[1][0].clone()], vec![first_seq[1][1].clone()],],
        "the left half checkpoints first, then the crash lands on the right half"
    );
    // A fresh process: the finished left half must come from its checkpoint.
    let second = Arc::new(StagedLlm::new());
    second
        .incomplete_when_ids_at_least
        .store(2, Ordering::SeqCst);
    let outcome = staged_consolidator(&store, wiki.clone(), second.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("the resume must finish from checkpoints");
    assert_eq!(
        second.calls().0,
        3,
        "reused: the first block and the finished left half cost no call"
    );
    let seq = second.map_requests();
    assert_eq!(
        seq.iter().map(Vec::len).collect::<Vec<_>>(),
        vec![2, 1, 1],
        "only the parent, the unfinished half and the last block are re-run"
    );
    assert_eq!(
        seq[0], first_seq[1],
        "control: the parent block re-runs (it never checkpointed)"
    );
    assert_eq!(seq[1], vec![seq[0][1].clone()], "the right half runs");
    assert_eq!(
        wiki.read_page(ws, proj, &outcome.path).unwrap().body,
        "Grounded by the evidence section."
    );
    assert_eq!(
        page_source_count(&store, outcome.page_id.unwrap()),
        3,
        "every cited id reaches page_sources after the resume"
    );
}

#[tokio::test]
async fn chunked_failed_parent_block_records_no_checkpoint() {
    // The split halves checkpoint; the block that failed coverage never
    // may, or a resume would reuse the very result that failed validation.
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900, 900]).await;
    let llm = Arc::new(StagedLlm::new());
    llm.incomplete_when_ids_at_least.store(2, Ordering::SeqCst);
    staged_consolidator(&store, wiki, llm.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("recovered run publishes");
    // The publish prunes every checkpoint, so re-run against a store where
    // publication fails after the map: the surviving rows tell the story.
    let tmp2 = tempfile::tempdir().unwrap();
    let (store2, wiki2, session2, ws2, proj2) =
        chunked_fixture(tmp2.path(), &[900, 900, 900]).await;
    let llm2 = Arc::new(StagedLlm::new());
    llm2.incomplete_when_ids_at_least.store(2, Ordering::SeqCst);
    llm2.fail_final.store(true, Ordering::SeqCst);
    staged_consolidator(&store2, wiki2, llm2.clone(), ws2, proj2, 6_000)
        .consolidate_session(
            session2,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect_err("the interrupted run must fail");
    assert_eq!(llm2.calls().0, 5);
    let cfg = ChunkingConfig {
        target_chars: 6_000 * 3 * 8 / 10,
        model: "staged".into(),
    };
    let parts = plan_map_parts(
        &store2
            .reader
            .observations_for_session_in_scope(ws2, proj2, session2)
            .await
            .unwrap(),
    );
    let rows = store2
        .writer
        .load_consolidation_chunks(ws2, proj2, session2)
        .await
        .unwrap();
    let keys: Vec<String> = rows.into_iter().map(|r| r.chunk_fingerprint).collect();
    let parent_parts = vec![parts[1].clone(), parts[2].clone()];
    assert!(
        !keys.contains(&map_block_fingerprint(&cfg, &parent_parts)),
        "the failed parent block must not checkpoint"
    );
    assert!(
        keys.contains(&map_block_fingerprint(&cfg, &[parts[0].clone()])),
        "control: the complete single-part block checkpoints"
    );
    assert!(keys.contains(&map_subblock_fingerprint(&cfg, &[parts[1].clone()])));
    assert!(keys.contains(&map_subblock_fingerprint(&cfg, &[parts[2].clone()])));
}

#[tokio::test]
async fn chunked_single_part_incomplete_coverage_normalizes_and_publishes() {
    // Every reply drops one id: a one-part block cannot split, so its hole
    // becomes `no_durable_fact_ids` and the session still gets its page.
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900]).await;
    let llm = Arc::new(StagedLlm::new());
    llm.incomplete_when_ids_at_least.store(1, Ordering::SeqCst);
    let outcome = staged_consolidator(&store, wiki.clone(), llm.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("a one-part coverage hole must normalize, not fail the session");
    assert_eq!(
        llm.calls(),
        (4, 0, 1),
        "block 1 normalizes; block 2 splits into two normalizing halves"
    );
    let page = wiki
        .read_page(ws, proj, &outcome.path)
        .expect("the page is written even when nothing survives");
    assert!(page.frontmatter.get(CONSOLIDATION_MARKER_KEY).is_some());
    assert_eq!(
        page_source_count(&store, outcome.page_id.unwrap()),
        0,
        "ids omitted everywhere never enter page_sources"
    );
}

#[tokio::test]
async fn chunked_reduce_incomplete_coverage_normalizes_and_publishes() {
    let tmp = tempfile::tempdir().unwrap();
    let sizes: Vec<usize> = (0..200).map(|_| 60).collect();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &sizes).await;
    let total = store
        .reader
        .observations_for_session_in_scope(ws, proj, session)
        .await
        .unwrap()
        .len();
    let llm = Arc::new(StagedLlm::new());
    llm.incomplete_reduce.store(true, Ordering::SeqCst);
    let outcome = staged_consolidator(&store, wiki.clone(), llm.clone(), ws, proj, 1_400)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("a reduce group that drops one id must normalize, not fail");
    let (maps, reduces, finals) = llm.calls();
    assert!(
        maps > 1 && reduces >= 1 && finals == 1,
        "got {maps} {reduces} {finals}"
    );
    let page = wiki
        .read_page(ws, proj, &outcome.path)
        .expect("the page is written after the normalized reduce");
    assert!(page.frontmatter.get(CONSOLIDATION_MARKER_KEY).is_some());
    assert_eq!(
        page_source_count(&store, outcome.page_id.unwrap()),
        total - 1,
        "the id dropped by the reduce group leaves the grounded set"
    );
}

#[tokio::test]
async fn chunked_duplicated_and_foreign_no_durable_ids_normalize() {
    // The first map reply answers a one-part block with zero extractions
    // and a `no_durable_fact_ids` that repeats the input id and names a
    // foreign one. Both defects normalize; the run must not fail.
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900]).await;
    let llm = Arc::new(StagedLlm::new());
    llm.weird_no_durable_map.store(true, Ordering::SeqCst);
    let outcome = staged_consolidator(&store, wiki.clone(), llm.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect("a duplicated or foreign no_durable id must normalize, not fail");
    assert_eq!(llm.calls(), (2, 0, 1));
    assert_eq!(
        page_source_count(&store, outcome.page_id.unwrap()),
        2,
        "the ids the second block still cites stay grounded"
    );
}

#[tokio::test]
async fn chunked_grounding_break_on_a_splittable_block_stays_terminal() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[900, 900, 900]).await;
    let llm = Arc::new(StagedLlm::new());
    // Call 2 is the two-part block: coverage recovery must not launder an
    // id cited outside the stage input.
    llm.bad_map_call.store(2, Ordering::SeqCst);
    let error = staged_consolidator(&store, wiki.clone(), llm.clone(), ws, proj, 6_000)
        .consolidate_session(
            session,
            false,
            ai_memory_core::ActorContext::anonymous(),
            None,
            None,
        )
        .await
        .expect_err("a cited id outside the stage input must fail closed");
    assert!(matches!(error, ConsolidatorError::UngroundedExtractions(_)));
    assert_eq!(
        llm.calls().0,
        2,
        "no split retries run for a grounding break"
    );
    assert_eq!(llm.calls().2, 0);
    assert_eq!(
        redacted_error_summary(&error),
        "consolidation failed: class=ungrounded-extractions status=none"
    );
    let path = PagePath::new(format!("sessions/{session}.md")).unwrap();
    assert!(
        wiki.read_page(ws, proj, &path).is_err(),
        "no page may be published when grounding fails"
    );
}

#[test]
fn normalize_coverage_repairs_only_coverage_and_passes_validation() {
    let allowed: HashSet<String> = ["id-1".to_owned(), "id-2".to_owned(), "id-3".to_owned()].into();
    let extraction = |ids: &[&str]| EvidenceExtraction {
        observation_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
        kind: ExtractionKind::Fact,
        title: "t".into(),
        summary: "s".into(),
        body_markdown: "b".into(),
        tags: Vec::new(),
        entities: Vec::new(),
        confidence: 0.9,
    };
    // id-3 is unaccounted (joins), id-1 is cited AND listed (leaves), the
    // listed duplicate and the foreign id drop out.
    let mut result = ExtractionResult {
        extractions: vec![extraction(&["id-1"])],
        rationale: String::new(),
        no_durable_fact_ids: vec![
            "id-1".to_owned(),
            "id-2".to_owned(),
            "id-2".to_owned(),
            FOREIGN_OBSERVATION_ID.to_owned(),
        ],
    };
    let adjusted = normalize_coverage(&mut result, &allowed);
    assert_eq!(adjusted, 4, "three drops plus one addition");
    assert_eq!(
        result.no_durable_fact_ids,
        vec!["id-2".to_owned(), "id-3".to_owned()],
        "legit entries keep their place, missing ids append sorted"
    );
    validate_extraction_result(&result, &allowed)
        .expect("a normalized result passes the validator verbatim");
    assert_eq!(
        result.extractions[0].observation_ids,
        vec!["id-1".to_owned()],
        "extractions are untouched: grounding cannot be laundered"
    );
    // Control: an already-normalized result is byte-identical and free.
    let mut clean = ExtractionResult {
        extractions: result.extractions.clone(),
        rationale: String::new(),
        no_durable_fact_ids: result.no_durable_fact_ids.clone(),
    };
    assert_eq!(normalize_coverage(&mut clean, &allowed), 0);
    assert_eq!(clean.no_durable_fact_ids, result.no_durable_fact_ids);
    // Control: an extraction citing a foreign id still fails after repair.
    let mut ungrounded = ExtractionResult {
        extractions: vec![extraction(&["id-9"])],
        rationale: String::new(),
        no_durable_fact_ids: Vec::new(),
    };
    normalize_coverage(&mut ungrounded, &allowed);
    assert!(matches!(
        validate_extraction_result(&ungrounded, &allowed),
        Err(ConsolidatorError::UngroundedExtractions(_))
    ));
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

#[test]
fn map_and_reduce_system_prompts_enforce_security_boundary_and_accurate_id_header() {
    for (name, prompt) in [("map", MAP_SYSTEM_PROMPT), ("reduce", REDUCE_SYSTEM_PROMPT)] {
        assert!(
            prompt.contains("## SECURITY BOUNDARY"),
            "{name} prompt must contain ## SECURITY BOUNDARY"
        );
        assert!(
            prompt.contains("untrusted") && prompt.contains("not instructions"),
            "{name} prompt must frame input as untrusted data, not instructions"
        );
    }
    assert!(
        !MAP_SYSTEM_PROMPT.contains("`id:` lines"),
        "MAP_SYSTEM_PROMPT must not refer to nonexistent `id:` lines"
    );
    assert!(
        MAP_SYSTEM_PROMPT.contains("--- observation <id>"),
        "MAP_SYSTEM_PROMPT must reference the actual `--- observation <id> ... ---` header"
    );
}

#[tokio::test]
async fn final_request_fits_checks_unclipped_evidence_and_output_language() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[1]).await;
    let llm = Arc::new(StagedLlm::new());
    let consolidator = staged_consolidator(&store, wiki.clone(), llm.clone(), ws, proj, 4_000);

    // Oversized evidence: previously `build_final_request_single` clipped it
    // before measuring, making `final_request_fits` falsely return `true`.
    let oversized: Vec<EvidenceExtraction> = (0..30)
        .map(|i| EvidenceExtraction {
            observation_ids: vec![format!("obs-{i}")],
            kind: ExtractionKind::Fact,
            title: format!("Fact {i}"),
            summary: format!("Summary {i}"),
            body_markdown: "x".repeat(1_800),
            tags: Vec::new(),
            entities: Vec::new(),
            confidence: 0.9,
        })
        .collect();
    assert!(
        !consolidator
            .final_request_fits(session, &oversized, "", None, &[])
            .unwrap(),
        "unclipped evidence exceeding max_input_chars must not report as fitting"
    );

    // Exact boundary check for `with_output_language`: pick a budget that fits
    // the base `SYSTEM_PROMPT` but overflows once `output_language` is appended.
    let single = vec![EvidenceExtraction {
        observation_ids: vec!["obs-1".into()],
        kind: ExtractionKind::Fact,
        title: "One".into(),
        summary: "Short".into(),
        body_markdown: "y".repeat(200),
        tags: Vec::new(),
        entities: Vec::new(),
        confidence: 0.9,
    }];
    let base_prompt = crate::with_output_language(SYSTEM_PROMPT, None);
    let lang_prompt = crate::with_output_language(SYSTEM_PROMPT, Some("Portuguese (Brazil)"));
    assert!(count_chars(&lang_prompt) > count_chars(&base_prompt));
    let base_total = {
        let (header, evidence, suffix) = render_final_single_parts(
            session,
            &single,
            "",
            None,
            PromptBudgets::from_limits_with_margin(100_000, 32_000, 0.8),
            &[],
            &base_prompt,
        );
        count_chars(&base_prompt)
            + count_chars(&header)
            + count_chars(&evidence)
            + count_chars(&suffix)
            + schema_chars::<ConsolidatedPage>()
            + PROMPT_ENVELOPE_RESERVE_CHARS
    };
    let tight_budgets = PromptBudgets {
        max_input_chars: base_total,
        max_output_tokens: 4_096,
    };
    let mut base_consolidator = Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        llm.clone(),
        ws,
        proj,
    );
    base_consolidator.budgets = tight_budgets;
    assert!(
        base_consolidator
            .final_request_fits(session, &single, "", None, &[])
            .unwrap(),
        "control: exact budget fits without output_language"
    );
    let lang_consolidator =
        base_consolidator.with_output_language(Some("Portuguese (Brazil)".into()));
    assert!(
        !lang_consolidator
            .final_request_fits(session, &single, "", None, &[])
            .unwrap(),
        "output_language suffix pushing past max_input_chars must report as not fitting"
    );
}

#[test]
fn plan_map_blocks_rejects_input_exceeding_max_map_blocks() {
    let observations: Vec<Observation> = (0..=MAX_MAP_BLOCKS)
        .map(|i| chunk_observation(format!("block-{i} ").repeat(200)))
        .collect();
    let cfg = ChunkingConfig {
        target_chars: 1_500,
        model: "m1".into(),
    };
    let err = plan_map_blocks(SessionId::new(), &observations, &cfg, 20_000)
        .err()
        .expect("exceeding MAX_MAP_BLOCKS must fail closed");
    assert!(matches!(err, ConsolidatorError::ChunkDoesNotFit));
}

/// Provider whose reduce stage echoes two distinct extractions back verbatim
/// without reducing either count or rendered size, verifying the strict-shrink
/// guard fails closed on the first non-shrinking round.
struct NonShrinkingReduceLlm {
    reduce_calls: AtomicUsize,
}

#[async_trait::async_trait]
impl LlmProvider for NonShrinkingReduceLlm {
    fn name(&self) -> &'static str {
        "non-shrinking"
    }
    fn model(&self) -> &str {
        "non-shrinking"
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
        let user = &request.messages[0].content;
        if request
            .system
            .as_deref()
            .is_some_and(|s| s.contains("reduce stage"))
        {
            self.reduce_calls.fetch_add(1, Ordering::SeqCst);
            let ids = observation_ids_in_reduce_request(user);
            let extractions: Vec<serde_json::Value> = ids
                .into_iter()
                .map(|id| {
                    serde_json::json!({
                        "observation_ids": [id],
                        "kind": "fact",
                        "title": "Echoed extraction",
                        "summary": "Unchanged summary.",
                        "body_markdown": "z".repeat(1_800),
                        "confidence": 0.9,
                    })
                })
                .collect();
            return Ok(serde_json::json!({
                "extractions": extractions,
                "rationale": "",
                "no_durable_fact_ids": [],
            }));
        }
        unreachable!("test invokes chunked_reduce_extractions directly")
    }
}

#[tokio::test]
async fn chunked_reduce_fails_closed_when_round_makes_no_progress() {
    let tmp = tempfile::tempdir().unwrap();
    let (store, wiki, session, ws, proj) = chunked_fixture(tmp.path(), &[1]).await;
    let llm = Arc::new(NonShrinkingReduceLlm {
        reduce_calls: AtomicUsize::new(0),
    });
    let consolidator = Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki,
        llm.clone(),
        ws,
        proj,
    )
    .with_prompt_limits(6_000, 32_000, 0.8)
    .with_chunking(4_000, 0.8);
    let extractions: Vec<EvidenceExtraction> = (0..10)
        .map(|i| EvidenceExtraction {
            observation_ids: vec![format!("00000000-0000-0000-0000-{i:012}")],
            kind: ExtractionKind::Fact,
            title: "Echoed extraction".into(),
            summary: "Unchanged summary.".into(),
            body_markdown: "z".repeat(1_800),
            tags: Vec::new(),
            entities: Vec::new(),
            confidence: 0.9,
        })
        .collect();
    let run = ChunkedRun {
        ws,
        proj,
        session,
        observations: Vec::new(),
        operation_id: LlmOperationId::new(),
    };
    let err = consolidator
        .chunked_reduce_extractions(&run, extractions, "", None, &[])
        .await
        .expect_err("a reduce round that shrinks neither count nor size must fail closed");
    assert!(matches!(err, ConsolidatorError::ChunkDoesNotFit));
    assert!(
        llm.reduce_calls.load(Ordering::SeqCst) < MAX_REDUCE_DEPTH,
        "strict-shrink check must stop after the first non-shrinking round, not burn all {MAX_REDUCE_DEPTH} depths"
    );
}
