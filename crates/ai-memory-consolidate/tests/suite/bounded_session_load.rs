//! Bounded session-observation load: the consolidation and auto-improve
//! paths must produce byte-identical prompts to the old full-load path,
//! refuse (fail closed) sessions with no observations in their scope, and
//! keep at most the selection's rows resident at a million-row session.

use std::sync::{Arc, Mutex};

use ai_memory_core::{ActorContext, PagePath, ProjectId, SessionId, Tier, WorkspaceId};
use ai_memory_llm::{ChatRequest, LlmProvider, LlmResult};
use ai_memory_store::{Store, StoreError, StoredPageBody};
use ai_memory_wiki::{Wiki, WritePageRequest};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::auto_improve::auto_improve_oracle::{
    oracle_build_prompt_input, oracle_preflight_rejection, oracle_session_duration_secs,
};
use crate::auto_improve::{
    MAX_OBSERVATION_BODY_CHARS, PatchablePageContext, PromptSections, REVIEW_INTERPOSE,
    auto_improve_prompt_sections, estimate_tokens, load_patchable_pages, load_rejection_context,
};
use crate::consolidator::PromptBudgets;
use crate::consolidator::consolidator_oracle::{oracle_batch_request, oracle_single_request};
use crate::consolidator::frozen_projection_oracle as frozen;
use crate::projection::{
    BoundedProjectionStats, ObservationProjectionConfig, ProjectedObservations,
    project_session_observations,
};
use crate::{
    AutoImproveReviewConfig, Consolidator, ConsolidatorError, existing_session_page_titles,
    run_auto_improve_review,
};

/// The SQLite file behind a data dir opened by [`Store::open`].
fn db_file(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("db").join(ai_memory_store::DB_FILENAME)
}

/// Locates the first differing byte between two prompts so an equivalence
/// failure points at the drift instead of dumping both sides.
fn first_diff(a: &str, b: &str) -> String {
    let (ba, bb) = (a.as_bytes(), b.as_bytes());
    let n = ba.len().min(bb.len());
    let i = (0..n).find(|&i| ba[i] != bb[i]).unwrap_or(n);
    let region = |s: &[u8]| {
        let mut start = i.saturating_sub(60);
        while start < i && matches!(s[start], 0x80..=0xBF) {
            start += 1;
        }
        let mut end = (i + 60).min(s.len());
        while end > i && matches!(s[end], 0x80..=0xBF) {
            end -= 1;
        }
        format!("...{}...", String::from_utf8_lossy(&s[start..end]))
    };
    format!(
        "first diff at byte {i} (lens {} vs {}): new={} old={}",
        a.len(),
        b.len(),
        region(ba),
        region(bb)
    )
}

/// Capturing LLM: records every request and answers with a minimal valid
/// structured payload for whichever shape the caller asked for.
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

    async fn complete(&self, _request: ChatRequest) -> LlmResult<ai_memory_llm::ChatResponse> {
        unreachable!("both paths use structured completion")
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        _schema: Value,
    ) -> LlmResult<Value> {
        self.requests.lock().unwrap().push(request);
        // One payload that parses as ConsolidatedPage, ConsolidatedBatch and
        // AutoImproveLlmResponse alike: empty lists + defaults.
        Ok(json!({
            "title": "Fixed page",
            "body_markdown": "Bounded-load equivalence page body.",
            "tags": [],
            "updates": [],
            "rationale": "bounded load equivalence",
            "summary": "reviewed",
            "proposals": [],
            "rejected_candidates": []
        }))
    }
}

/// Deterministic fixture row for index `idx` of `total`: a mix of kinds,
/// importances, high-signal and routing text, code fences and one
/// near-16-KiB body per ~97 rows. `bulk` pads every body to ~4 KiB so the
/// character budget prunes the selection, not just the count cap, so the
/// selection, pruning and residue all have material.
fn fixture_row(idx: usize, total: usize, bulk: bool) -> (&'static str, String, String, u8) {
    let (kind, title, body, importance) = if idx == 0 {
        ("session-start", "start".into(), "cwd".into(), 1)
    } else if idx == total - 1 {
        ("session-end", "end".into(), "done".into(), 1)
    } else if idx % 97 == 50 {
        // 16.3k chars: at the 16 KiB observation cap, over the excerpt,
        // so the block is truncated and the residue gets a tail.
        (
            "post-tool-use",
            "long tool output".into(),
            format!(
                "tool_family: shell\n---\n{}\nsrc/long/file-{}.rs",
                "x".repeat(16_300),
                idx
            ),
            2,
        )
    } else if idx % 40 == 5 {
        (
            "user-prompt",
            "prompt".into(),
            format!("Please fix the regression in module {idx}; decided on caching."),
            5,
        )
    } else if idx % 40 == 20 {
        (
            "pre-compact",
            "compact".into(),
            "context pressure".into(),
            5,
        )
    } else if idx % 37 == 10 {
        (
            "stop",
            "stop".into(),
            format!("assistant note: the decision at step {idx} holds"),
            6,
        )
    } else if idx % 31 == 7 {
        ("post-compaction", "post".into(), "summary".into(), 4)
    } else if idx % 25 == 3 {
        (
            "post-tool-use",
            "tool".into(),
            format!("tool_family: shell\n---\nsrc/gen/file-{idx}.rs"),
            1,
        )
    } else if idx % 53 == 11 {
        (
            "notification",
            "routing".into(),
            "note about long-term memory (ai-memory) routing install".into(),
            2,
        )
    } else if idx % 29 == 13 {
        (
            "other",
            "misc".into(),
            "key decision always never".into(),
            9,
        )
    } else {
        ("post-tool-use", "routine".into(), "ok".into(), 1)
    };
    if bulk && kind != "session-start" && kind != "session-end" {
        (
            kind,
            title,
            format!("{body}\n{}", "y".repeat(3_900)),
            importance,
        )
    } else {
        (kind, title, body, importance)
    }
}

/// Seed one session of `count` observations directly in SQLite (one
/// transaction, explicit `created_at` one minute apart, a deliberate tie
/// between rows 77 and 78 to pin the `(created_at, rowid)` order) plus the
/// pages the request builders read: the session page (when `session_page`)
/// and two content pages. `bulk` pads bodies to ~4 KiB so the character
/// budget prunes the selection. Returns the store and coordinates.
async fn seeded_fixture(
    tmp: &std::path::Path,
    count: usize,
    bulk: bool,
    session_page: bool,
) -> (Store, Wiki, WorkspaceId, ProjectId, SessionId) {
    seeded_fixture_uniform(tmp, count, bulk, session_page, false).await
}

/// `uniform = true` seeds a monotonous session: every middle row is the same
/// routine tool output. All scores then differ only by recency (a step of
/// ~0.27/row), so the 256-of-300 selection boundary cuts through the
/// +20 even-sample bonus zone — the shape that makes the bonus decisive
/// and any drift in it observable.
async fn seeded_fixture_uniform(
    tmp: &std::path::Path,
    count: usize,
    bulk: bool,
    session_page: bool,
    uniform: bool,
) -> (Store, Wiki, WorkspaceId, ProjectId, SessionId) {
    let store = Store::open(tmp).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "bounded", None)
        .await
        .unwrap();
    let sid = SessionId::new();

    const BASE: i64 = 1_700_000_000_000_000; // µs
    {
        let conn = rusqlite::Connection::open(db_file(tmp)).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
             VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
            rusqlite::params![sid.as_bytes(), ws.as_bytes(), proj.as_bytes(), "/w", BASE],
        )
        .unwrap();
        let mut ins = tx
            .prepare_cached(
                "INSERT INTO observations \
                 (id, session_id, workspace_id, project_id, kind, title, body, importance, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .unwrap();
        for idx in 0..count {
            let (kind, title, body, importance) = fixture_row(idx, count, bulk);
            let (kind, title, body, importance) = if uniform && (1..count - 1).contains(&idx) {
                ("post-tool-use", "routine".to_string(), "ok".to_string(), 1)
            } else {
                (kind, title, body, importance)
            };
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(idx as u64).to_be_bytes());
            id[8..16].copy_from_slice(&((idx as u64).wrapping_mul(7919)).to_be_bytes());
            // Rows 77 and 78 share a created_at: order must fall back to rowid.
            let slot = if idx == 78 { 77 } else { idx };
            let created_at = BASE + slot as i64 * 60_000_000;
            ins.execute(rusqlite::params![
                &id[..],
                sid.as_bytes(),
                ws.as_bytes(),
                proj.as_bytes(),
                kind,
                title,
                body,
                importance as i64,
                created_at
            ])
            .unwrap();
        }
        drop(ins);
        tx.commit().unwrap();
    }

    let wiki = Wiki::new(tmp, store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    if session_page {
        let session_page = PagePath::new(format!("sessions/{sid}.md")).unwrap();
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: session_page,
            frontmatter: serde_json::json!({"title": "Prior session page"}),
            body: "Old session page body.\n\nPrior narrative line.".into(),
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
    for (path, title) in [
        ("concepts/alpha.md", "Alpha concept"),
        ("decisions/bravo.md", "Bravo decision"),
    ] {
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).unwrap(),
            frontmatter: serde_json::json!({"title": title}),
            body: format!("{title} body."),
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

    (store, wiki, ws, proj, sid)
}

/// The inputs the production builders read, computed the same way the
/// production code does: current session-page body, per-call instructions
/// (unchanged by scrub/clip/trim for this plain text) and the
/// existing-page titles.
async fn builder_inputs(
    wiki: &Wiki,
    reader: &ai_memory_store::ReaderPool,
    ws: WorkspaceId,
    proj: ProjectId,
    sid: SessionId,
) -> (String, Option<String>, Vec<String>) {
    let path = PagePath::new(format!("sessions/{sid}.md")).unwrap();
    let current_body = wiki
        .read_page(ws, proj, &path)
        .map(|md| md.body)
        .unwrap_or_default();
    let instructions: Option<String> = Some("Prefer concise titles.".to_string());
    let titles =
        existing_session_page_titles(reader, ws, proj, &ActorContext::anonymous(), sid).await;
    (current_body, instructions, titles)
}

/// Byte-identical prompts: the oracle (old full-load code) and the bounded
/// snapshot path must agree for single-page consolidation below and above
/// the selection cap, with the residue off and on, and with the character
/// budget pruning the selection (`bulk`) at and above the cap.
#[tokio::test]
async fn single_page_prompt_is_byte_identical_to_the_old_full_load_path() {
    for (count, residue, bulk) in [
        (100usize, false, false),
        (100, true, false),
        (300, false, false),
        (300, true, false),
        (256, false, true),
        (300, false, true),
        (300, true, true),
    ] {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj, sid) = seeded_fixture(tmp.path(), count, bulk, true).await;
        let (current_body, instructions, titles) =
            builder_inputs(&wiki, &store.reader, ws, proj, sid).await;
        let actor = ActorContext::anonymous();

        // Old path: the full load plus the verbatim pre-change builder.
        let observations = store.reader.observations_for_session(sid).await.unwrap();
        assert_eq!(observations.len(), count);
        let oracle = oracle_single_request(
            sid,
            &observations,
            &current_body,
            instructions.as_deref(),
            PromptBudgets::default(),
            &titles,
            residue,
        );

        // New path: the bounded snapshot load through the consolidator.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
            requests: requests.clone(),
        });
        Consolidator::new(
            store.reader.clone(),
            store.writer.clone(),
            wiki.clone(),
            llm,
            ws,
            proj,
        )
        .with_verbatim_residue(residue)
        .consolidate_session(sid, false, actor, None, instructions.as_deref())
        .await
        .unwrap();

        let captured = requests.lock().unwrap();
        assert_eq!(
            captured.len(),
            1,
            "one LLM call for count={count} bulk={bulk}"
        );
        let new = &captured[0];
        assert_eq!(new.system, oracle.system, "system prompt drift");
        assert_eq!(
            new.messages[0].content,
            oracle.messages[0].content,
            "user message drift (count={count}, residue={residue}, bulk={bulk}): {}",
            first_diff(&new.messages[0].content, &oracle.messages[0].content)
        );
        assert_eq!(new.max_tokens, oracle.max_tokens);
        assert_eq!(new.temperature, oracle.temperature);
        if bulk {
            // The budget must have pruned below the 256 cap, so this case
            // exercises pruning, not just the count cap.
            assert!(
                !new.messages[0]
                    .content
                    .contains(&format!("selected 256 of {count}")),
                "bulk case must prune below the cap (count={count})"
            );
        }
    }
}

/// Uniform (monotonous) session: scores differ only by recency, so the
/// even-sample bonus sits at the selection boundary. Byte-identical to the
/// old full-load path — a mutant that drops or moves the bonus drifts here.
#[tokio::test]
async fn uniform_session_selection_is_byte_identical_to_the_old_full_load_path() {
    let tmp = TempDir::new().unwrap();
    let (store, wiki, ws, proj, sid) =
        seeded_fixture_uniform(tmp.path(), 300, false, true, true).await;
    let (current_body, instructions, titles) =
        builder_inputs(&wiki, &store.reader, ws, proj, sid).await;
    let actor = ActorContext::anonymous();

    let observations = store.reader.observations_for_session(sid).await.unwrap();
    assert_eq!(observations.len(), 300);
    let oracle = oracle_single_request(
        sid,
        &observations,
        &current_body,
        instructions.as_deref(),
        PromptBudgets::default(),
        &titles,
        false,
    );

    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests.clone(),
    });
    Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki,
        llm,
        ws,
        proj,
    )
    .consolidate_session(sid, false, actor, None, instructions.as_deref())
    .await
    .unwrap();

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let new = &captured[0];
    assert_eq!(new.system, oracle.system, "system prompt drift");
    assert_eq!(
        new.messages[0].content,
        oracle.messages[0].content,
        "uniform-session prompt drift: {}",
        first_diff(&new.messages[0].content, &oracle.messages[0].content)
    );
    assert_eq!(new.max_tokens, oracle.max_tokens);
    assert_eq!(new.temperature, oracle.temperature);
}

/// Same guarantee for the batch (multi-page) request builder, including a
/// bulk case where the character budget prunes the selection.
#[tokio::test]
async fn batch_prompt_is_byte_identical_to_the_old_full_load_path() {
    for (count, bulk) in [(100usize, false), (300, false), (300, true)] {
        let tmp = TempDir::new().unwrap();
        let (store, wiki, ws, proj, sid) = seeded_fixture(tmp.path(), count, bulk, true).await;
        let actor = ActorContext::anonymous();

        let observations = store.reader.observations_for_session(sid).await.unwrap();
        let titles = existing_session_page_titles(&store.reader, ws, proj, &actor, sid).await;
        let oracle = oracle_batch_request(
            sid,
            &observations,
            &[],
            Some("Prefer concise titles."),
            PromptBudgets::default(),
            &titles,
            false,
        );

        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
            requests: requests.clone(),
        });
        Consolidator::new(
            store.reader.clone(),
            store.writer.clone(),
            wiki,
            llm,
            ws,
            proj,
        )
        .consolidate_session_multi(sid, false, actor, None, Some("Prefer concise titles."))
        .await
        .unwrap();

        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].system, oracle.system);
        assert_eq!(
            captured[0].messages[0].content,
            oracle.messages[0].content,
            "batch user message drift (count={count}, bulk={bulk}): {}",
            first_diff(
                &captured[0].messages[0].content,
                &oracle.messages[0].content
            )
        );
        if bulk {
            assert!(
                !captured[0].messages[0]
                    .content
                    .contains("selected 256 of 300"),
                "bulk batch case must prune below the cap"
            );
        }
    }
}

/// The auto-improve review prompt and report are byte-identical to the old
/// full-load path: above the sample limit (streaming), at the limit with a
/// session page (48, fetch-all) and at the limit without one (72, fetch-all),
/// and the preflight rejection report agrees on a tiny session.
#[tokio::test]
async fn auto_improve_prompt_and_report_match_the_old_full_load_path() {
    for (count, session_page) in [(300usize, true), (48, true), (72, false)] {
        let tmp = TempDir::new().unwrap();
        let (store, _wiki, ws, proj, sid) =
            seeded_fixture(tmp.path(), count, false, session_page).await;
        let cfg = AutoImproveReviewConfig {
            max_input_tokens: 12_000,
            ..AutoImproveReviewConfig::default()
        };

        // Old path inputs, read exactly like the production code does.
        let observations = store.reader.observations_for_session(sid).await.unwrap();
        let duration = oracle_session_duration_secs(&observations);
        assert!(
            oracle_preflight_rejection(&observations, duration, &cfg).is_none(),
            "fixture must pass the old preflight (count={count})"
        );
        let session_page_body: Option<StoredPageBody> = store
            .reader
            .page_body_by_ids(ws, proj, &format!("sessions/{sid}.md"))
            .await
            .unwrap();
        assert_eq!(session_page_body.is_some(), session_page);
        let briefing = store
            .reader
            .briefing_for_project(ws, proj, 100, ai_memory_core::OwnerFilter::Any, false)
            .await
            .unwrap();
        let recent: Vec<_> = briefing
            .recent_pages
            .iter()
            .filter(|page| !page.path.starts_with("sessions/"))
            .cloned()
            .collect();
        let patchable: Vec<PatchablePageContext> = Vec::new();
        let rejections: Vec<ai_memory_store::AutoImproveRejectionSummary> = Vec::new();
        let old_prompt = oracle_build_prompt_input(
            sid,
            &observations,
            duration,
            session_page_body.as_ref(),
            &recent,
            &patchable,
            &rejections,
            &cfg,
        );

        // New path.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
            requests: requests.clone(),
        });
        let report =
            run_auto_improve_review(&store.reader, llm.as_ref(), ws, proj, sid, cfg.clone())
                .await
                .unwrap();

        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 1, "one LLM call for count={count}");
        assert_eq!(
            captured[0].messages[0].content,
            old_prompt.prompt,
            "auto-improve prompt drift (count={count}, page={session_page}): {}",
            first_diff(&captured[0].messages[0].content, &old_prompt.prompt)
        );

        // Report fields the old path computed from the full load.
        assert_eq!(report.session_id, sid.to_string());
        assert_eq!(
            report.observations_considered,
            observations.len(),
            "count must come from the full population, not the selection"
        );
        assert_eq!(report.session_duration_secs, duration);
        assert_eq!(
            report.estimated_input_tokens,
            estimate_tokens(&old_prompt.prompt)
        );
        assert_eq!(report.provider, "capturing");
        assert_eq!(report.model, "capturing");
        assert_eq!(report.min_confidence, cfg.min_confidence);
        assert_eq!(report.proposal_actor, cfg.proposal_actor);
        assert_eq!(report.pending_path, cfg.pending_path);
        assert_eq!(report.warnings, old_prompt.warnings);
        assert_eq!(
            report.rejected_candidates.len(),
            old_prompt.rejected_candidates.len(),
        );
        for (new_c, old_c) in report
            .rejected_candidates
            .iter()
            .zip(old_prompt.rejected_candidates.iter())
        {
            assert_eq!(new_c.reason, old_c.reason);
            assert_eq!(new_c.evidence, old_c.evidence);
        }

        // Above the limit (300 rows, limit 48) the sampler must name the
        // full population; at the limit (48/72) there is no sampling.
        let sampled: Vec<_> = report
            .rejected_candidates
            .iter()
            .filter(|c| c.reason == "input_budget_sampled")
            .collect();
        if count > 72 {
            assert_eq!(sampled.len(), 1, "300 rows must exceed the sample limit");
            assert!(
                sampled[0]
                    .evidence
                    .contains(&format!("of {count} observations"))
            );
        } else {
            assert!(sampled.is_empty(), "at the limit there is no sampling");
        }
    }

    // Preflight rejection on a tiny session: same report, no LLM call.
    let tmp2 = TempDir::new().unwrap();
    let (store2, _wiki2, ws2, proj2, sid2) = seeded_fixture(tmp2.path(), 3, false, true).await;
    let cfg = AutoImproveReviewConfig {
        max_input_tokens: 12_000,
        ..AutoImproveReviewConfig::default()
    };
    let observations2 = store2.reader.observations_for_session(sid2).await.unwrap();
    let duration2 = oracle_session_duration_secs(&observations2);
    let old_rejection = oracle_preflight_rejection(&observations2, duration2, &cfg)
        .expect("tiny session must be rejected by the old preflight");
    let requests2 = Arc::new(Mutex::new(Vec::new()));
    let llm2: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests2.clone(),
    });
    let report2 = run_auto_improve_review(&store2.reader, llm2.as_ref(), ws2, proj2, sid2, cfg)
        .await
        .unwrap();
    assert!(
        requests2.lock().unwrap().is_empty(),
        "no LLM call after preflight"
    );
    assert_eq!(report2.observations_considered, 3);
    assert_eq!(report2.session_duration_secs, duration2);
    assert_eq!(report2.summary, "session skipped by preflight filters");
    assert_eq!(report2.provider, "none");
    assert_eq!(report2.rejected_candidates.len(), 1);
    assert_eq!(report2.rejected_candidates[0].reason, old_rejection.reason);
    assert_eq!(
        report2.rejected_candidates[0].evidence,
        old_rejection.evidence
    );
}

/// A purged session fails closed with EmptySession and never reaches the
/// LLM, through both consolidation entry points.
#[tokio::test]
async fn purged_session_is_refused_without_an_llm_call() {
    let tmp = TempDir::new().unwrap();
    let (store, wiki, ws, proj, sid) = seeded_fixture(tmp.path(), 50, false, true).await;

    // Purge the session's observations (the sessions row stays).
    {
        let conn = rusqlite::Connection::open(db_file(tmp.path()).as_path()).unwrap();
        conn.execute(
            "DELETE FROM observations WHERE session_id = ?1",
            rusqlite::params![sid.as_bytes()],
        )
        .unwrap();
    }

    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests.clone(),
    });
    let consolidator = Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki,
        llm,
        ws,
        proj,
    );

    let err = consolidator
        .consolidate_session(sid, false, ActorContext::anonymous(), None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ConsolidatorError::EmptySession(id) if id == sid),
        "single-page path must fail closed: {err:?}"
    );
    let err = consolidator
        .consolidate_session_multi(sid, false, ActorContext::anonymous(), None, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, ConsolidatorError::EmptySession(id) if id == sid),
        "multi-page path must fail closed: {err:?}"
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "no LLM call for a purged session"
    );
}

/// The observations-gated reader refuses a foreign scope and admits the
/// session's own scope (the legitimate control).
#[tokio::test]
async fn gated_reader_refuses_foreign_scopes() {
    let tmp = TempDir::new().unwrap();
    let (store, _wiki, ws, proj, sid) = seeded_fixture(tmp.path(), 12, false, true).await;

    // Legitimate control: the session's own scope opens.
    let totals = store
        .reader
        .read_session_observations_in_scope(ws, proj, sid, |snap| snap.projection_totals())
        .await
        .unwrap();
    assert_eq!(totals.total, 12);

    // A foreign workspace must be refused, not answered.
    let foreign_ws = store
        .writer
        .get_or_create_workspace("foreign")
        .await
        .unwrap();
    let foreign_proj = store
        .writer
        .get_or_create_project(ws, "foreign", None)
        .await
        .unwrap();
    let err = store
        .reader
        .read_session_observations_in_scope(foreign_ws, proj, sid, |snap| snap.projection_totals())
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::SessionCollision),
        "foreign scope must be refused: {err:?}"
    );
    let err = store
        .reader
        .read_session_observations_in_scope(ws, foreign_proj, sid, |snap| snap.projection_totals())
        .await
        .unwrap_err();
    assert!(
        matches!(err, StoreError::SessionCollision),
        "foreign project must be refused: {err:?}"
    );
}

/// The auto-improve review still refuses a foreign-scope session through its
/// pre-existing `session_project_ids` gate, with no rows loaded and no LLM
/// call; a missing session is refused as `SessionNotFound`.
#[tokio::test]
async fn auto_improve_refuses_foreign_scope_without_an_llm_call() {
    let tmp = TempDir::new().unwrap();
    let (store, _wiki, ws, proj, sid) = seeded_fixture(tmp.path(), 3, false, true).await;
    let foreign_proj = store
        .writer
        .get_or_create_project(ws, "foreign", None)
        .await
        .unwrap();

    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests.clone(),
    });
    let cfg = AutoImproveReviewConfig::default();

    let err = run_auto_improve_review(
        &store.reader,
        llm.as_ref(),
        ws,
        foreign_proj,
        sid,
        cfg.clone(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, crate::AutoImproveError::SessionOutOfScope { .. }),
        "a foreign project must be refused out of scope: {err:?}"
    );
    let err = run_auto_improve_review(&store.reader, llm.as_ref(), ws, proj, SessionId::new(), cfg)
        .await
        .unwrap_err();
    assert!(
        matches!(err, crate::AutoImproveError::SessionNotFound(_)),
        "a missing session must be refused: {err:?}"
    );
    assert!(
        requests.lock().unwrap().is_empty(),
        "no LLM call may happen"
    );
}

/// A purge committed while the review's single snapshot read is in flight
/// must not split the report from the prompt. The old path read the
/// preflight totals from one closed transaction and the dump from a second
/// one, so a purge between the two left the report on the pre-purge
/// generation and the prompt on the post-purge one (100 versus 1). Both now
/// come out of one read transaction: the seam below commits a real DELETE
/// of 99 of the 100 rows between the totals aggregate and the preflight
/// decision, and the report and the prompt must still agree.
#[tokio::test]
async fn auto_improve_review_uses_one_generation_of_the_session() {
    let tmp = TempDir::new().unwrap();
    let (store, _wiki, ws, proj, sid) = seeded_fixture(tmp.path(), 100, false, true).await;
    let db = db_file(tmp.path());

    // Commit the purge through a second connection, inside the review's
    // single snapshot read.
    REVIEW_INTERPOSE.lock().unwrap().push((
        sid,
        Box::new(move || {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute(
                "DELETE FROM observations WHERE session_id = ?1 AND rowid NOT IN \
                 (SELECT MIN(rowid) FROM observations WHERE session_id = ?2)",
                rusqlite::params![sid.as_bytes(), sid.as_bytes()],
            )
            .unwrap();
        }),
    ));

    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests.clone(),
    });
    let cfg = AutoImproveReviewConfig {
        max_input_tokens: 12_000,
        ..AutoImproveReviewConfig::default()
    };
    let report = run_auto_improve_review(&store.reader, llm.as_ref(), ws, proj, sid, cfg)
        .await
        .unwrap();

    assert!(
        !REVIEW_INTERPOSE
            .lock()
            .unwrap()
            .iter()
            .any(|(owner, _)| *owner == sid),
        "the review of this session must have run the seam"
    );
    let current_rows = store.reader.observations_for_session(sid).await.unwrap();
    assert_eq!(current_rows.len(), 1, "the purge must have committed");
    assert_eq!(
        report.observations_considered, 100,
        "the report must keep the pre-purge population"
    );
    assert_eq!(
        report.session_duration_secs, 5940,
        "the duration must keep the pre-purge span"
    );
    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1, "one LLM call for the review");
    let prompt = &captured[0].messages[0].content;
    assert!(
        prompt.contains("--- observation 100/100 ---"),
        "the dump must keep the pre-purge last anchor"
    );
    let stated = prompt
        .find("Observation count: ")
        .map(|at| &prompt[at + "Observation count: ".len()..])
        .and_then(|rest| {
            rest.split(['\n', ' '])
                .next()
                .and_then(|n| n.parse::<usize>().ok())
        })
        .expect("the prompt must state the observation count");
    assert_eq!(
        report.observations_considered, stated,
        "report and prompt must describe one generation: report={} prompt={}",
        report.observations_considered, stated
    );
}

/// The snapshot seam belongs to one session: a review of another session,
/// as a test running in parallel would start, must leave it in place, and
/// the owner's review must still run it.
#[tokio::test]
async fn auto_improve_review_seam_is_never_consumed_by_another_session() {
    let owner_tmp = TempDir::new().unwrap();
    let (owner_store, _owner_wiki, owner_ws, owner_proj, owner_sid) =
        seeded_fixture(owner_tmp.path(), 100, false, true).await;
    let other_tmp = TempDir::new().unwrap();
    let (other_store, _other_wiki, other_ws, other_proj, other_sid) =
        seeded_fixture(other_tmp.path(), 100, false, true).await;

    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ran_in_seam = ran.clone();
    REVIEW_INTERPOSE.lock().unwrap().push((
        owner_sid,
        Box::new(move || ran_in_seam.store(true, std::sync::atomic::Ordering::SeqCst)),
    ));

    let other_llm = CapturingLlm {
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    run_auto_improve_review(
        &other_store.reader,
        &other_llm,
        other_ws,
        other_proj,
        other_sid,
        AutoImproveReviewConfig::default(),
    )
    .await
    .unwrap();
    assert!(
        !ran.load(std::sync::atomic::Ordering::SeqCst),
        "another session's review must not run the owner's seam"
    );

    let owner_llm = CapturingLlm {
        requests: Arc::new(Mutex::new(Vec::new())),
    };
    run_auto_improve_review(
        &owner_store.reader,
        &owner_llm,
        owner_ws,
        owner_proj,
        owner_sid,
        AutoImproveReviewConfig::default(),
    )
    .await
    .unwrap();
    assert!(
        ran.load(std::sync::atomic::Ordering::SeqCst),
        "the owner's review must run its seam"
    );
}

/// A present session with no observations and no preflight minimums still
/// gets reviewed with the empty `(none)` projection, exactly as the old
/// full-load path did: the preflight sees zero rows from the session's own
/// generation and passes, and the review runs. Truly missing ids keep
/// their `SessionNotFound` refusal (the control above).
#[tokio::test]
async fn auto_improve_review_of_a_rowless_session_with_preflight_off() {
    let tmp = TempDir::new().unwrap();
    let (store, _wiki, ws, proj, sid) = seeded_fixture(tmp.path(), 0, false, false).await;
    let cfg = AutoImproveReviewConfig {
        min_observations: 0,
        min_session_duration_secs: 0,
        ..AutoImproveReviewConfig::default()
    };

    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests.clone(),
    });
    let report = run_auto_improve_review(&store.reader, llm.as_ref(), ws, proj, sid, cfg)
        .await
        .unwrap();

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1, "the review must run, not fail closed");
    assert!(
        captured[0].messages[0]
            .content
            .contains("Observation count: 0"),
        "the prompt must state the rowless generation"
    );
    assert!(
        captured[0].messages[0].content.contains("(none)"),
        "the empty projection's (none) dump must reach the prompt"
    );
    assert_eq!(report.observations_considered, 0);
    assert!(
        report.summary.contains("review completed"),
        "a rowless review completes like any other: {}",
        report.summary
    );
}

/// The zero-observation-budget early return must reach the streaming path:
/// above the sample cap with a token budget that leaves the observation
/// dump no budget at all, the prompt must carry the old no-budget marker
/// byte-for-byte like the old full-load path, not an empty dump. (The
/// budget override reaches the production caller through the MCP server's
/// `max_input_tokens` override.)
#[tokio::test]
async fn zero_budget_streaming_path_keeps_the_old_no_budget_marker() {
    let tmp = TempDir::new().unwrap();
    let (store, _wiki, ws, proj, sid) = seeded_fixture(tmp.path(), 300, false, true).await;
    let cfg = AutoImproveReviewConfig {
        max_input_tokens: 1_000,
        ..AutoImproveReviewConfig::default()
    };

    // Old path: the full load through the verbatim pre-change builder.
    let observations = store.reader.observations_for_session(sid).await.unwrap();
    assert_eq!(observations.len(), 300);
    let duration = oracle_session_duration_secs(&observations);
    let session_page: Option<StoredPageBody> = store
        .reader
        .page_body_by_ids(ws, proj, &format!("sessions/{sid}.md"))
        .await
        .unwrap();
    let briefing = store
        .reader
        .briefing_for_project(ws, proj, 100, ai_memory_core::OwnerFilter::Any, false)
        .await
        .unwrap();
    let recent: Vec<_> = briefing
        .recent_pages
        .iter()
        .filter(|page| !page.path.starts_with("sessions/"))
        .cloned()
        .collect();
    let old_prompt = oracle_build_prompt_input(
        sid,
        &observations,
        duration,
        session_page.as_ref(),
        &recent,
        &[],
        &[],
        &cfg,
    );
    assert!(
        old_prompt.prompt.contains("no projection budget"),
        "the fixture must hit the zero-budget marker on the old path"
    );

    // New path: 300 rows stream past the sample cap with the same zero
    // budget and must render the same marker.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests.clone(),
    });
    let report = run_auto_improve_review(&store.reader, llm.as_ref(), ws, proj, sid, cfg)
        .await
        .unwrap();

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1, "one LLM call for the review");
    assert_eq!(
        captured[0].messages[0].content,
        old_prompt.prompt,
        "zero-budget prompt drift: {}",
        first_diff(&captured[0].messages[0].content, &old_prompt.prompt)
    );
    assert!(
        captured[0].messages[0]
            .content
            .contains("no projection budget"),
        "the streaming path must keep the old no-budget marker"
    );
    assert_eq!(report.observations_considered, 300);
}

/// The old preflight's duration refusal, kept by the new single-snapshot
/// read: a minimum far above the session's span rejects with exactly the
/// old report — same reason, same evidence — and no LLM call.
#[tokio::test]
async fn duration_preflight_matches_the_old_refusal() {
    let tmp = TempDir::new().unwrap();
    let (store, _wiki, ws, proj, sid) = seeded_fixture(tmp.path(), 100, false, true).await;
    let cfg = AutoImproveReviewConfig {
        min_session_duration_secs: u64::MAX,
        ..AutoImproveReviewConfig::default()
    };
    let observations = store.reader.observations_for_session(sid).await.unwrap();
    let old = oracle_preflight_rejection(
        &observations,
        oracle_session_duration_secs(&observations),
        &cfg,
    )
    .expect("the span must fall short of the minimum");
    assert_eq!(old.reason, "session_too_short");

    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests.clone(),
    });
    let report = run_auto_improve_review(&store.reader, llm.as_ref(), ws, proj, sid, cfg)
        .await
        .unwrap();
    assert!(
        requests.lock().unwrap().is_empty(),
        "no LLM call after the duration preflight"
    );
    assert!(!report.rejected_candidates.is_empty());
    assert_eq!(report.rejected_candidates[0].reason, old.reason);
    assert_eq!(report.rejected_candidates[0].evidence, old.evidence);
}

/// The frozen base projection at the sample-cap edges — exactly at the cap
/// (in-memory branch), one row over and well over it (streaming branch) —
/// with the residue on and the residue budget swept through 4096, 65536
/// and 1_000_000 chars, mixed and uniform sessions alike: the new snapshot
/// path must agree with the old full-load rule on selection and text.
#[tokio::test]
async fn projection_at_the_256_257_edges_and_residue_budgets_matches_the_frozen_base() {
    for (count, uniform) in [(256usize, false), (257, false), (300, false), (300, true)] {
        let tmp = TempDir::new().unwrap();
        let (store, _wiki, ws, proj, sid) =
            seeded_fixture_uniform(tmp.path(), count, false, true, uniform).await;
        let observations = store.reader.observations_for_session(sid).await.unwrap();
        for budget in [4096usize, 65536, 1_000_000] {
            let frozen = frozen::project_observations(
                &observations,
                &frozen::ObservationProjectionConfig::new(budget, 256, 64)
                    .with_verbatim_residue(true),
            );
            let current_cfg =
                ObservationProjectionConfig::new(budget, 256, 64).with_verbatim_residue(true);
            let (current, _stats) = store
                .reader
                .read_session_observations_in_scope(ws, proj, sid, move |snap| {
                    project_session_observations(snap, &current_cfg)
                })
                .await
                .unwrap()
                .expect("rows must project, not fail closed");
            assert_eq!(
                current.selected_indices, frozen.selected_indices,
                "selection drift at count={count} budget={budget}"
            );
            assert!(
                current.text == frozen.text,
                "text drift at count={count} budget={budget}: {}",
                first_diff(&current.text, &frozen.text)
            );
        }
    }
}

/// The frozen base projection — a verbatim copy of the pre-change
/// `projection.rs` — and the current in-memory projection must agree
/// field for field. The oracles above call the current projection, so
/// only this frozen copy (and the literals in the projection unit tests)
/// can catch a drift of the shared rule's values.
#[tokio::test]
async fn current_projection_matches_the_frozen_base_projection() {
    for (count, residue) in [(100usize, false), (100, true), (300, false), (300, true)] {
        let tmp = TempDir::new().unwrap();
        let (store, _wiki, _ws, _proj, sid) = seeded_fixture(tmp.path(), count, false, true).await;
        let observations = store.reader.observations_for_session(sid).await.unwrap();
        assert_eq!(observations.len(), count);

        let frozen = frozen::project_observations(
            &observations,
            &frozen::ObservationProjectionConfig::new(1_000_000, 256, 3_000)
                .with_context_label("frozen")
                .with_verbatim_residue(residue),
        );
        let current = crate::projection::project_observations(
            &observations,
            &ObservationProjectionConfig::new(1_000_000, 256, 3_000)
                .with_context_label("frozen")
                .with_verbatim_residue(residue),
        );
        assert_eq!(
            frozen.text, current.text,
            "in-memory text drift (count={count}, residue={residue})"
        );
        assert_eq!(
            frozen.selected_indices, current.selected_indices,
            "in-memory selection drift (count={count})"
        );
        assert_eq!(
            frozen.warnings, current.warnings,
            "in-memory warning drift (count={count})"
        );
        assert_eq!(frozen.total_count, current.total_count);
        assert_eq!(frozen.selected_count, current.selected_count);
        assert_eq!(frozen.omitted_count, current.omitted_count);
        assert_eq!(frozen.truncated_bodies, current.truncated_bodies);
    }
}

/// The streaming projection against the frozen base at the sample-cap
/// edges: exactly at the cap (the in-memory branch), one row over it and
/// well over it (the streaming branch) — the new snapshot path must
/// render exactly what the old full-load projection rendered, selection
/// and text alike.
#[tokio::test]
async fn streaming_projection_matches_the_frozen_base_projection() {
    for (count, limit) in [(256usize, 256usize), (257, 256), (300, 256)] {
        let tmp = TempDir::new().unwrap();
        let (store, _wiki, ws, proj, sid) = seeded_fixture(tmp.path(), count, false, true).await;
        let observations = store.reader.observations_for_session(sid).await.unwrap();
        for residue in [false, true] {
            let frozen = frozen::project_observations(
                &observations,
                &frozen::ObservationProjectionConfig::new(1_000_000, limit, 3_000)
                    .with_context_label("frozen")
                    .with_verbatim_residue(residue),
            );
            let current_cfg = ObservationProjectionConfig::new(1_000_000, limit, 3_000)
                .with_context_label("frozen")
                .with_verbatim_residue(residue);
            let (current, _stats) = store
                .reader
                .read_session_observations_in_scope(ws, proj, sid, move |snap| {
                    project_session_observations(snap, &current_cfg)
                })
                .await
                .unwrap()
                .expect("rows must project, not fail closed");
            assert_eq!(
                frozen.text, current.text,
                "streaming text drift (count={count}, limit={limit}, residue={residue})"
            );
            assert_eq!(
                frozen.selected_indices, current.selected_indices,
                "streaming selection drift (count={count})"
            );
            assert_eq!(
                frozen.warnings, current.warnings,
                "streaming warning drift (count={count})"
            );
            assert_eq!(frozen.total_count, current.total_count);
            assert_eq!(frozen.selected_count, current.selected_count);
            assert_eq!(frozen.omitted_count, current.omitted_count);
            assert_eq!(frozen.truncated_bodies, current.truncated_bodies);
        }
    }
}

/// A session whose `sessions` row froze on a stale project still consolidates
/// into the project its observations actually landed in, and the gate passes
/// there. (The schema forbids a sessions row in another workspace, so the
/// stale coordinate is the project.)
#[tokio::test]
async fn stale_sessions_row_does_not_divert_the_write() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj_new = store
        .writer
        .get_or_create_project(ws, "bounded", None)
        .await
        .unwrap();
    let proj_stale = store
        .writer
        .get_or_create_project(ws, "stale", None)
        .await
        .unwrap();
    let sid = SessionId::new();

    // Sessions row in the STALE project; every observation in the new one.
    const BASE: i64 = 1_700_000_000_000_000;
    {
        let conn = rusqlite::Connection::open(db_file(tmp.path()).as_path()).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
             VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
            rusqlite::params![
                sid.as_bytes(),
                ws.as_bytes(),
                proj_stale.as_bytes(),
                "/w",
                BASE
            ],
        )
        .unwrap();
        for idx in 0..20 {
            let (kind, title, body, importance) = fixture_row(idx, 20, false);
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(idx as u64).to_be_bytes());
            id[8..16].copy_from_slice(&((idx as u64).wrapping_mul(7919)).to_be_bytes());
            conn.execute(
                "INSERT INTO observations \
                 (id, session_id, workspace_id, project_id, kind, title, body, importance, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    &id[..],
                    sid.as_bytes(),
                    ws.as_bytes(),
                    proj_new.as_bytes(),
                    kind,
                    title,
                    body,
                    importance as i64,
                    BASE + idx as i64 * 60_000_000
                ],
            )
            .unwrap();
        }
    }

    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests.clone(),
    });
    Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        llm,
        ws,
        proj_new,
    )
    .consolidate_session(sid, false, ActorContext::anonymous(), None, None)
    .await
    .unwrap();

    // Written into the observations' project, not the stale row's.
    let written = store
        .reader
        .page_body_by_ids(ws, proj_new, &format!("sessions/{sid}.md"))
        .await
        .unwrap()
        .expect("page must land in the observations' scope");
    assert_eq!(written.title, "Fixed page");
    assert!(
        store
            .reader
            .page_body_by_ids(ws, proj_stale, &format!("sessions/{sid}.md"))
            .await
            .unwrap()
            .is_none(),
        "nothing may be written to the stale scope"
    );
    assert_eq!(requests.lock().unwrap().len(), 1);
}

/// A hybrid session whose observations landed in two projects is consolidated
/// strictly within the project it is bound to: the bounded path reads only
/// that project's rows, so project B's observations never reach a project-A
/// consolidation's prompt (GHSA-xfgc-hc4c-wj7h). The oracle is built from
/// project A's rows alone, and the dump's row count and project-B content
/// prove the other scope is absent.
#[tokio::test]
async fn hybrid_two_project_session_prompt_is_scoped_to_its_project() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj_a = store
        .writer
        .get_or_create_project(ws, "project-a", None)
        .await
        .unwrap();
    let proj_b = store
        .writer
        .get_or_create_project(ws, "project-b", None)
        .await
        .unwrap();
    let sid = SessionId::new();

    // 200 rows in project A, 100 in project B; the sessions row in A.
    const BASE: i64 = 1_700_000_000_000_000;
    const COUNT_A: usize = 200;
    const COUNT_B: usize = 100;
    {
        let conn = rusqlite::Connection::open(db_file(tmp.path()).as_path()).unwrap();
        conn.execute(
            "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
             VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
            rusqlite::params![sid.as_bytes(), ws.as_bytes(), proj_a.as_bytes(), "/w", BASE],
        )
        .unwrap();
        for idx in 0..(COUNT_A + COUNT_B) {
            let (kind, title, body, importance) = fixture_row(idx, COUNT_A + COUNT_B, false);
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(idx as u64).to_be_bytes());
            id[8..16].copy_from_slice(&((idx as u64).wrapping_mul(7919)).to_be_bytes());
            let proj = if idx < COUNT_A { proj_a } else { proj_b };
            conn.execute(
                "INSERT INTO observations \
                 (id, session_id, workspace_id, project_id, kind, title, body, importance, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    &id[..],
                    sid.as_bytes(),
                    ws.as_bytes(),
                    proj.as_bytes(),
                    kind,
                    title,
                    body,
                    importance as i64,
                    BASE + idx as i64 * 60_000_000
                ],
            )
            .unwrap();
        }
    }

    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    let (current_body, instructions, titles) =
        builder_inputs(&wiki, &store.reader, ws, proj_a, sid).await;

    // The oracle is project A's rows alone: a consolidation bound to project
    // A must read only those, never the session's project-B rows.
    let all = store.reader.observations_for_session(sid).await.unwrap();
    assert_eq!(all.len(), COUNT_A + COUNT_B);
    let project_a_only: Vec<_> = all.into_iter().filter(|o| o.project_id == proj_a).collect();
    assert_eq!(project_a_only.len(), COUNT_A);
    let oracle = oracle_single_request(
        sid,
        &project_a_only,
        &current_body,
        instructions.as_deref(),
        PromptBudgets::default(),
        &titles,
        false,
    );

    // The bounded path gates on, and reads, only project A's rows.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
        requests: requests.clone(),
    });
    Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        llm,
        ws,
        proj_a,
    )
    .consolidate_session(
        sid,
        false,
        ActorContext::anonymous(),
        None,
        instructions.as_deref(),
    )
    .await
    .unwrap();

    let captured = requests.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(
        captured[0].messages[0].content,
        oracle.messages[0].content,
        "hybrid session prompt drift: {}",
        first_diff(
            &captured[0].messages[0].content,
            &oracle.messages[0].content
        )
    );

    // Byte-equality with the project-A-only oracle already proves the prompt
    // holds exactly project A's projection; the dump's row count confirms the
    // hybrid session's project-B rows did not inflate it.
    let dump = &captured[0].messages[0].content;
    assert!(
        dump.contains(&format!("--- observation {COUNT_A}/{COUNT_A} ---")),
        "the dump is scoped to project A's row count"
    );
    assert!(
        !dump.contains(&format!("/{} ---", COUNT_A + COUNT_B)),
        "the hybrid session's project-B rows must not inflate the count"
    );
}

/// The streaming scan of a 100k-row session keeps at most the selection's
/// rows resident (256 + 1 for consolidation, 72 + 1 for auto-improve) while
/// reading every row once, and does so in bounded time.
mod slow {
    use super::*;

    async fn seed_hundred_k(
        tmp: &std::path::Path,
        ws: WorkspaceId,
        proj: ProjectId,
        sid: SessionId,
    ) {
        const BASE: i64 = 1_700_000_000_000_000;
        let conn = rusqlite::Connection::open(db_file(tmp)).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
             VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
            rusqlite::params![sid.as_bytes(), ws.as_bytes(), proj.as_bytes(), "/w", BASE],
        )
        .unwrap();
        let mut ins = tx
            .prepare_cached(
                "INSERT INTO observations \
                 (id, session_id, workspace_id, project_id, kind, title, body, importance, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .unwrap();
        for idx in 0..100_000usize {
            let (kind, title, body, importance) = fixture_row(idx, 100_000, false);
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(idx as u64).to_be_bytes());
            id[8..16].copy_from_slice(&((idx as u64).wrapping_mul(7919)).to_be_bytes());
            ins.execute(rusqlite::params![
                &id[..],
                sid.as_bytes(),
                ws.as_bytes(),
                proj.as_bytes(),
                kind,
                title,
                body,
                importance as i64,
                BASE + idx as i64 * 60_000_000
            ])
            .unwrap();
        }
        drop(ins);
        tx.commit().unwrap();
    }

    #[tokio::test]
    async fn hundred_k_row_session_stays_within_the_selection_residency() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "bounded", None)
            .await
            .unwrap();
        let sid = SessionId::new();
        seed_hundred_k(tmp.path(), ws, proj, sid).await;

        // Consolidation shape: cap 256.
        let cfg =
            ObservationProjectionConfig::new(1_000_000, 256, 3_000).with_context_label("row-bound");
        let start = std::time::Instant::now();
        let (selected, stats) = store
            .reader
            .read_session_observations(ws, proj, sid, move |snap| {
                project_session_observations(snap, &cfg)
            })
            .await
            .unwrap()
            .unwrap();
        let elapsed = start.elapsed();
        eprintln!(
            "bounded projection of 100k rows: wall {:.1}s, rows_read {}, resident_high_water {}",
            elapsed.as_secs_f64(),
            stats.rows_read,
            stats.resident_high_water
        );
        assert_eq!(stats.rows_read, 100_000 + selected.selected_count);
        assert!(
            stats.resident_high_water <= 257,
            "consolidation must hold at most 256 + 1 rows resident, held {}",
            stats.resident_high_water
        );
        assert!(
            stats.heap_high_water <= 256,
            "the streaming top-k heap must stay at most the selection limit, held {}",
            stats.heap_high_water
        );
        assert_eq!(
            stats.residue_candidates_high_water, 0,
            "residue off must collect no per-candidate tuples"
        );
        assert!(selected.selected_count <= 256);
        assert_eq!(selected.total_count, 100_000);
        assert!(selected.text.contains("observations omitted"));

        // Auto-improve shape: cap 72.
        let cfg =
            ObservationProjectionConfig::new(1_000_000, 72, 1_500).with_context_label("row-bound");
        let (selected, stats) = store
            .reader
            .read_session_observations(ws, proj, sid, move |snap| {
                project_session_observations(snap, &cfg)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(
            stats.resident_high_water <= 73,
            "auto-improve must hold at most 72 + 1 rows resident, held {}",
            stats.resident_high_water
        );
        assert!(
            stats.heap_high_water <= 72,
            "the streaming top-k heap must stay at most the selection limit, held {}",
            stats.heap_high_water
        );
        assert_eq!(
            stats.residue_candidates_high_water, 0,
            "residue off must collect no per-candidate tuples"
        );
        assert!(selected.selected_count <= 72);
    }

    /// The production auto-improve bounded-read inputs, derived the same way
    /// `run_auto_improve_review` derives them: the briefing minus `sessions/`
    /// pages, the session page body, the patchable pages and the rejection
    /// context. The test's projection config is therefore the one production
    /// builds, not a hand-tuned one.
    async fn auto_improve_sections(
        reader: &ai_memory_store::ReaderPool,
        ws: WorkspaceId,
        proj: ProjectId,
        sid: SessionId,
        cfg: &AutoImproveReviewConfig,
    ) -> PromptSections {
        let briefing = reader
            .briefing_for_project(ws, proj, 100, ai_memory_core::OwnerFilter::Any, false)
            .await
            .unwrap();
        let reviewer_recent_pages: Vec<_> = briefing
            .recent_pages
            .iter()
            .filter(|page| !page.path.starts_with("sessions/"))
            .cloned()
            .collect();
        let session_page = reader
            .page_body_by_ids(ws, proj, &format!("sessions/{sid}.md"))
            .await
            .unwrap();
        let patchable_pages = load_patchable_pages(reader, ws, proj, &reviewer_recent_pages, cfg)
            .await
            .unwrap();
        let rejection_context = load_rejection_context(reader, ws, proj, cfg).await.unwrap();
        auto_improve_prompt_sections(
            session_page.as_ref(),
            &reviewer_recent_pages,
            &[],
            &patchable_pages,
            &rejection_context,
            cfg,
        )
    }

    /// The exact bounded read production performs for auto-improve: the
    /// sections-derived projection config through `project_session_observations`,
    /// keeping the `BoundedProjectionStats` the production path discards.
    async fn auto_improve_bounded_read(
        store: &Store,
        ws: WorkspaceId,
        proj: ProjectId,
        sid: SessionId,
        sections: &PromptSections,
    ) -> (ProjectedObservations, BoundedProjectionStats) {
        let cfg = ObservationProjectionConfig::new(
            sections.observation_budget,
            sections.selected_limit,
            MAX_OBSERVATION_BODY_CHARS,
        )
        .with_context_label("auto-improve");
        store
            .reader
            .read_session_observations(ws, proj, sid, move |snap| {
                project_session_observations(snap, &cfg)
            })
            .await
            .unwrap()
            .expect("100k rows must project, not fail closed")
    }

    /// Auto-improve's residency bound at a 100,000-row session: the production
    /// path must hold at most `selected_limit + 1` full rows resident, the
    /// selection must never exceed the limit, and the prompt's "Observation
    /// count" must be the aggregate total, not the selected count. Both
    /// limits (72 without a session page, 48 with one) are exercised from a
    /// single 100k seed: the page is written between the two shapes.
    #[tokio::test]
    async fn hundred_k_row_auto_improve_stays_within_the_selection_residency() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "bounded", None)
            .await
            .unwrap();
        let sid = SessionId::new();
        seed_hundred_k(tmp.path(), ws, proj, sid).await;
        let cfg = AutoImproveReviewConfig::default();

        // Shape one: no session page -> limit 72.
        let sections = auto_improve_sections(&store.reader, ws, proj, sid, &cfg).await;
        assert_eq!(sections.selected_limit, 72, "no session page -> limit 72");
        let start = std::time::Instant::now();
        let (selected, stats) = auto_improve_bounded_read(&store, ws, proj, sid, &sections).await;
        let elapsed = start.elapsed();
        eprintln!(
            "auto-improve projection of 100k rows (no session page): wall {:.1}s, rows_read {}, resident_high_water {}",
            elapsed.as_secs_f64(),
            stats.rows_read,
            stats.resident_high_water
        );
        assert!(
            stats.resident_high_water <= 73,
            "auto-improve must hold at most 72 + 1 rows resident, held {}",
            stats.resident_high_water
        );
        assert!(
            selected.selected_count <= 72,
            "the selection must never exceed the limit, held {}",
            selected.selected_count
        );
        assert_eq!(stats.rows_read, 100_000 + selected.selected_count);
        assert_eq!(selected.total_count, 100_000);

        // Write the session page: the production limit drops to 48.
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(format!("sessions/{sid}.md")).unwrap(),
            frontmatter: serde_json::json!({"title": "Prior session page"}),
            body: "Old session page body.\n\nPrior narrative line.".into(),
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

        // Shape two: session page present -> limit 48.
        let sections = auto_improve_sections(&store.reader, ws, proj, sid, &cfg).await;
        assert_eq!(
            sections.selected_limit, 48,
            "session page present -> limit 48"
        );
        let start = std::time::Instant::now();
        let (selected, stats) = auto_improve_bounded_read(&store, ws, proj, sid, &sections).await;
        let elapsed = start.elapsed();
        eprintln!(
            "auto-improve projection of 100k rows (with session page): wall {:.1}s, rows_read {}, resident_high_water {}",
            elapsed.as_secs_f64(),
            stats.rows_read,
            stats.resident_high_water
        );
        assert!(
            stats.resident_high_water <= 49,
            "auto-improve must hold at most 48 + 1 rows resident, held {}",
            stats.resident_high_water
        );
        assert!(
            selected.selected_count <= 48,
            "the selection must never exceed the limit, held {}",
            selected.selected_count
        );
        assert_eq!(stats.rows_read, 100_000 + selected.selected_count);
        assert_eq!(selected.total_count, 100_000);

        // The production entry point: the prompt's "Observation count" is the
        // aggregate total (100000), never the selected count; and the dump
        // the production path builds stays within the cap — a removed top-k
        // cap would let the budget-sampling rejection and the prompt blocks
        // exceed it.
        let requests = Arc::new(Mutex::new(Vec::new()));
        let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
            requests: requests.clone(),
        });
        let report = run_auto_improve_review(&store.reader, llm.as_ref(), ws, proj, sid, cfg)
            .await
            .unwrap();
        assert_eq!(report.observations_considered, 100_000);
        let sampled = report
            .rejected_candidates
            .iter()
            .find(|candidate| candidate.reason == "input_budget_sampled")
            .expect(
                "100k rows far exceed the sample limit; the sampling rejection must be present",
            );
        assert_eq!(
            sampled.evidence, "selected 48 of 100000 observations for scalable review",
            "the production dump must stay within the 48 limit with a session page"
        );
        let captured = requests.lock().unwrap();
        assert_eq!(captured.len(), 1, "one LLM call for the review");
        assert!(
            captured[0].messages[0]
                .content
                .contains("Observation count: 100000"),
            "the prompt must state the aggregate total, not the selected count"
        );
        let blocks = captured[0].messages[0]
            .content
            .matches("--- observation ")
            .count();
        assert!(
            blocks <= 48,
            "the prompt dump must hold at most 48 observation blocks, held {blocks}"
        );
    }

    /// Seed one session of `count` observations (one transaction, the same
    /// kind mix as [`super::fixture_row`], small bodies except the regular
    /// near-16-KiB row) plus nothing else — the pages the production paths
    /// read are written by the caller.
    async fn seed_large(
        tmp: &std::path::Path,
        ws: WorkspaceId,
        proj: ProjectId,
        sid: SessionId,
        count: usize,
    ) {
        const BASE: i64 = 1_700_000_000_000_000;
        let conn = rusqlite::Connection::open(db_file(tmp)).unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        tx.execute(
            "INSERT INTO sessions (id, workspace_id, project_id, agent_kind, cwd, started_at) \
             VALUES (?1, ?2, ?3, 'claude-code', ?4, ?5)",
            rusqlite::params![sid.as_bytes(), ws.as_bytes(), proj.as_bytes(), "/w", BASE],
        )
        .unwrap();
        let mut ins = tx
            .prepare_cached(
                "INSERT INTO observations \
                 (id, session_id, workspace_id, project_id, kind, title, body, importance, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )
            .unwrap();
        for idx in 0..count {
            let (kind, title, body, importance) = fixture_row(idx, count, false);
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&(idx as u64).to_be_bytes());
            id[8..16].copy_from_slice(&((idx as u64).wrapping_mul(7919)).to_be_bytes());
            ins.execute(rusqlite::params![
                &id[..],
                sid.as_bytes(),
                ws.as_bytes(),
                proj.as_bytes(),
                kind,
                title,
                body,
                importance as i64,
                BASE + idx as i64 * 60_000_000
            ])
            .unwrap();
        }
        drop(ins);
        tx.commit().unwrap();
    }

    /// Byte identity at scale: at 150k, 100k and 5k rows the streaming
    /// projection must produce exactly the old full-load path's prompts —
    /// checked against all three oracles: the frozen base projection
    /// (independent copy of the old rule), the consolidator oracle
    /// (verbatim pre-change request builder) and the auto-improve oracle.
    #[tokio::test]
    async fn large_session_prompts_match_the_old_full_load_path() {
        for (count, residue) in [(150_000usize, false), (100_000, true), (5_000, false)] {
            let tmp = TempDir::new().unwrap();
            let store = Store::open(tmp.path()).unwrap();
            let ws = store
                .writer
                .get_or_create_workspace("default")
                .await
                .unwrap();
            let proj = store
                .writer
                .get_or_create_project(ws, "bounded", None)
                .await
                .unwrap();
            let sid = SessionId::new();
            seed_large(tmp.path(), ws, proj, sid, count).await;
            let wiki = Wiki::new(tmp.path(), store.writer.clone())
                .unwrap()
                .with_store_reader(store.reader.clone());
            wiki.write_page(WritePageRequest {
                workspace_id: ws,
                project_id: proj,
                path: PagePath::new(format!("sessions/{sid}.md")).unwrap(),
                frontmatter: serde_json::json!({"title": "Prior session page"}),
                body: "Old session page body.\n\nPrior narrative line.".into(),
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

            let observations = store.reader.observations_for_session(sid).await.unwrap();
            assert_eq!(observations.len(), count);

            // (a) The frozen base projection and the new streaming
            // projection agree, selection and text alike — the frozen
            // copy is the independent rule the other oracles share
            // through the current projection.
            let frozen = frozen::project_observations(
                &observations,
                &frozen::ObservationProjectionConfig::new(1_000_000, 256, 3_000)
                    .with_context_label("row-bound")
                    .with_verbatim_residue(residue),
            );
            let current_cfg = ObservationProjectionConfig::new(1_000_000, 256, 3_000)
                .with_context_label("row-bound")
                .with_verbatim_residue(residue);
            let (current, stats) = store
                .reader
                .read_session_observations_in_scope(ws, proj, sid, move |snap| {
                    project_session_observations(snap, &current_cfg)
                })
                .await
                .unwrap()
                .expect("rows must project, not fail closed");
            assert_eq!(
                frozen.text,
                current.text,
                "projection text drift at count={count} residue={residue}: {}",
                first_diff(&frozen.text, &current.text)
            );
            assert_eq!(
                frozen.selected_indices, current.selected_indices,
                "projection selection drift at count={count}"
            );
            assert_eq!(
                frozen.warnings, current.warnings,
                "projection warning drift at count={count}"
            );
            assert_eq!(frozen.total_count, current.total_count);
            assert_eq!(frozen.selected_count, current.selected_count);
            assert_eq!(frozen.omitted_count, current.omitted_count);
            assert_eq!(frozen.truncated_bodies, current.truncated_bodies);
            // The top-k residency the base full load never had: the
            // streaming heap never exceeds the selection cap, and with
            // residue off no per-candidate tuples are collected at all.
            assert!(
                stats.heap_high_water <= 254,
                "the top-k heap must stay bounded by the selection cap, held {}",
                stats.heap_high_water
            );
            if residue {
                assert!(stats.rows_read >= count + current.selected_count);
            } else {
                assert_eq!(
                    stats.rows_read,
                    count + current.selected_count,
                    "only the selected rows may be re-read"
                );
                assert_eq!(
                    stats.residue_candidates_high_water, 0,
                    "residue off must collect no candidates"
                );
            }

            // (b) The consolidator oracle: the verbatim pre-change
            // request builder over the full load must equal the prompt
            // the streaming path sent.
            let (current_body, instructions, titles) =
                builder_inputs(&wiki, &store.reader, ws, proj, sid).await;
            let oracle = oracle_single_request(
                sid,
                &observations,
                &current_body,
                instructions.as_deref(),
                PromptBudgets::default(),
                &titles,
                residue,
            );
            let requests = Arc::new(Mutex::new(Vec::new()));
            let llm: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
                requests: requests.clone(),
            });
            Consolidator::new(
                store.reader.clone(),
                store.writer.clone(),
                wiki.clone(),
                llm,
                ws,
                proj,
            )
            .with_verbatim_residue(residue)
            .consolidate_session(
                sid,
                false,
                ActorContext::anonymous(),
                None,
                instructions.as_deref(),
            )
            .await
            .unwrap();
            {
                let captured = requests.lock().unwrap();
                assert_eq!(captured.len(), 1, "one LLM call at count={count}");
                assert_eq!(
                    captured[0].system, oracle.system,
                    "system drift at count={count}"
                );
                assert_eq!(
                    captured[0].messages[0].content,
                    oracle.messages[0].content,
                    "consolidator prompt drift at count={count}: {}",
                    first_diff(
                        &captured[0].messages[0].content,
                        &oracle.messages[0].content
                    )
                );
            }

            // (c) The auto-improve oracle: same guarantee for the review
            // prompt and its report fields.
            let cfg = AutoImproveReviewConfig {
                max_input_tokens: 12_000,
                ..AutoImproveReviewConfig::default()
            };
            let duration = oracle_session_duration_secs(&observations);
            let session_page: Option<StoredPageBody> = store
                .reader
                .page_body_by_ids(ws, proj, &format!("sessions/{sid}.md"))
                .await
                .unwrap();
            let briefing = store
                .reader
                .briefing_for_project(ws, proj, 100, ai_memory_core::OwnerFilter::Any, false)
                .await
                .unwrap();
            let recent: Vec<_> = briefing
                .recent_pages
                .iter()
                .filter(|page| !page.path.starts_with("sessions/"))
                .cloned()
                .collect();
            let old_prompt = oracle_build_prompt_input(
                sid,
                &observations,
                duration,
                session_page.as_ref(),
                &recent,
                &[],
                &[],
                &cfg,
            );
            let requests2 = Arc::new(Mutex::new(Vec::new()));
            let llm2: Arc<dyn LlmProvider> = Arc::new(CapturingLlm {
                requests: requests2.clone(),
            });
            let report = run_auto_improve_review(&store.reader, llm2.as_ref(), ws, proj, sid, cfg)
                .await
                .unwrap();
            let captured2 = requests2.lock().unwrap();
            assert_eq!(captured2.len(), 1, "one review call at count={count}");
            assert_eq!(
                captured2[0].messages[0].content,
                old_prompt.prompt,
                "auto-improve prompt drift at count={count}: {}",
                first_diff(&captured2[0].messages[0].content, &old_prompt.prompt)
            );
            assert_eq!(report.observations_considered, count);
            assert_eq!(report.session_duration_secs, duration);
        }
    }
}
