//! `[retrieval] observation_stream` (R1): matching observation bodies vote,
//! once per session, for that session's latest `sessions/<id>.md` page as one
//! more RRF stream. On by default, and byte-identical to the pre-stream
//! ranking when turned off.

use ai_memory_core::{
    AgentKind, NewObservation, NewPage, NewSession, ObservationId, ObservationKind, PagePath,
    ProjectId, Sanitized, Sanitizer, SessionId, Tier, WorkspaceId,
};
use ai_memory_store::{
    HybridSearch, PageFilter, PageHit, RetrievalTuning, SearchExplain, Store, VectorQuery,
};

pub(crate) fn stream_on() -> RetrievalTuning {
    RetrievalTuning {
        observation_stream: true,
        ..RetrievalTuning::default()
    }
}

pub(crate) fn stream_off() -> RetrievalTuning {
    RetrievalTuning {
        observation_stream: false,
        ..RetrievalTuning::default()
    }
}

#[test]
fn the_stream_is_on_by_default() {
    assert_eq!(RetrievalTuning::default(), stream_on());
}

pub(crate) async fn scope(store: &Store, ws: &str, proj: &str) -> (WorkspaceId, ProjectId) {
    let ws = store
        .writer
        .get_or_create_workspace(ws.to_string())
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, proj.to_string(), None)
        .await
        .unwrap();
    (ws, proj)
}

pub(crate) async fn session(store: &Store, ws: WorkspaceId, proj: ProjectId) -> SessionId {
    let id = SessionId::new();
    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::ClaudeCode,
            cwd: Some("/repo".into()),
            actor_user: None,
            origin: None,
        })
        .await
        .unwrap();
    id
}

pub(crate) async fn observe(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    session_id: SessionId,
    body: &str,
) -> ObservationId {
    store
        .writer
        .insert_observation(Sanitized::new(
            NewObservation {
                occurred_at: None,
                session_id,
                workspace_id: ws,
                project_id: proj,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: body.into(),
                importance: 5,
                file_path: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .unwrap()
}

pub(crate) fn page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.into(),
        body: body.into(),
        tier: Tier::Episodic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

pub(crate) async fn session_page(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    session_id: SessionId,
    body: &str,
) {
    store
        .writer
        .upsert_page(page(ws, proj, &format!("sessions/{session_id}.md"), body))
        .await
        .unwrap();
}

pub(crate) async fn search(
    store: &Store,
    ws: WorkspaceId,
    proj: ProjectId,
    query: &str,
) -> Vec<(PageHit, SearchExplain)> {
    store
        .reader
        .hybrid_search_explained(
            ws,
            proj,
            query.to_string(),
            None,
            String::new(),
            String::new(),
            0,
            10,
            None,
            false,
        )
        .await
        .unwrap()
}

fn paths(hits: &[(PageHit, SearchExplain)]) -> Vec<String> {
    hits.iter()
        .map(|(h, _)| h.path.as_str().to_string())
        .collect()
}

const K: f64 = 60.0;

#[tokio::test]
async fn an_observation_hit_lifts_its_own_session_page() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store, "default", "app").await;
    let target = session(&store, ws, proj).await;
    let other = session(&store, ws, proj).await;
    // The session pages keep only a short excerpt: neither mentions the
    // evidence words.
    session_page(&store, ws, proj, target, "User asked about weekend plans.").await;
    session_page(&store, ws, proj, other, "User asked about weekend plans.").await;
    let evidence = observe(
        &store,
        ws,
        proj,
        target,
        "I finally bought the quasar telescope from the camera shop downtown",
    )
    .await;
    observe(&store, ws, proj, other, "we talked about gardening instead").await;

    // Off: the evidence is unreachable from pages.
    store.reader.set_retrieval_tuning(stream_off());
    assert!(
        search(&store, ws, proj, "quasar telescope")
            .await
            .is_empty()
    );

    store.reader.set_retrieval_tuning(stream_on());
    let hits = search(&store, ws, proj, "quasar telescope").await;
    let target_path = format!("sessions/{target}.md");
    assert_eq!(paths(&hits), vec![target_path], "only the right session");
    let (hit, details) = &hits[0];
    assert_eq!(details.observation_rank, Some(1));
    assert_eq!(details.observation_ids, vec![evidence]);
    assert!((details.rrf.observation - 1.0 / (K + 1.0)).abs() < 1e-12);
    assert!((details.fused - details.rrf.observation).abs() < 1e-12);
    // The page had no snippet of its own for these words, so the bounded,
    // already-sanitized observation excerpt explains the hit.
    assert!(
        ai_memory_store::highlighted_snippet_html(&hit.snippet).contains("<mark>"),
        "{}",
        hit.snippet
    );
    assert!(hit.snippet.contains("quasar"), "{}", hit.snippet);
    assert!(!hit.title.is_empty());
}

#[tokio::test]
async fn page_filtering_precedes_the_observation_candidate_cut() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, proj) = scope(&store, "default", "app").await;
    let excluded = session(&store, ws, proj).await;
    let eligible = session(&store, ws, proj).await;

    let mut excluded_page = page(
        ws,
        proj,
        &format!("sessions/{excluded}.md"),
        "excluded session",
    );
    excluded_page.frontmatter_json = serde_json::json!({"tags": ["excluded"]});
    store.writer.upsert_page(excluded_page).await.unwrap();
    let mut eligible_page = page(
        ws,
        proj,
        &format!("sessions/{eligible}.md"),
        "eligible session",
    );
    eligible_page.frontmatter_json = serde_json::json!({"tags": ["wanted"]});
    store.writer.upsert_page(eligible_page).await.unwrap();

    // limit=1 produces an 80-observation input window. These stronger rows
    // would fill that window if page eligibility were checked after LIMIT.
    for _ in 0..80 {
        observe(
            &store,
            ws,
            proj,
            excluded,
            "crowdneedle crowdneedle crowdneedle crowdneedle",
        )
        .await;
    }
    observe(&store, ws, proj, eligible, "crowdneedle").await;

    let (hits, _) = store
        .reader
        .hybrid_search_scopes(
            &[(ws, proj)],
            HybridSearch {
                query: "crowdneedle",
                vector: None,
                limit: 1,
                expiry_cutoff_us: None,
                include_superseded: false,
                explain: true,
                owner: &ai_memory_core::OwnerFilter::Any,
                filter: PageFilter {
                    tag: Some("wanted"),
                    ..PageFilter::default()
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "the eligible page must survive the row cut");
    assert_eq!(hits[0].0.path.as_str(), format!("sessions/{eligible}.md"));
    assert_eq!(
        hits[0]
            .1
            .as_ref()
            .and_then(|details| details.observation_rank),
        Some(1)
    );
}

#[tokio::test]
async fn a_session_counts_once_however_many_observations_match() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, proj) = scope(&store, "default", "app").await;
    let chatty = session(&store, ws, proj).await;
    let quiet = session(&store, ws, proj).await;
    session_page(&store, ws, proj, chatty, "chatty session").await;
    session_page(&store, ws, proj, quiet, "quiet session").await;
    let mut chatty_ids = Vec::new();
    for i in 0..6 {
        chatty_ids.push(
            observe(
                &store,
                ws,
                proj,
                chatty,
                &format!("marmoset mention number {i} with filler words"),
            )
            .await,
        );
    }
    observe(&store, ws, proj, quiet, "marmoset marmoset marmoset").await;

    let hits = search(&store, ws, proj, "marmoset").await;
    assert_eq!(hits.len(), 2, "{:?}", paths(&hits));
    let chatty_path = format!("sessions/{chatty}.md");
    let (_, chatty_details) = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == chatty_path)
        .unwrap();
    let rank = chatty_details.observation_rank.unwrap();
    // One contribution, at the session's own stream rank — six matching
    // observations do not add up to six votes.
    assert!(rank <= 2);
    assert!((chatty_details.rrf.observation - 1.0 / (K + rank as f64)).abs() < 1e-12);
    assert!((chatty_details.fused - chatty_details.rrf.observation).abs() < 1e-12);
    // The matched ids are bounded and all belong to the chatty session.
    assert!(!chatty_details.observation_ids.is_empty());
    assert!(chatty_details.observation_ids.len() <= 3);
    assert!(
        chatty_details
            .observation_ids
            .iter()
            .all(|id| chatty_ids.contains(id))
    );
    // Stream ranks are distinct per page: the stream ranks sessions, not
    // observations.
    let ranks: Vec<usize> = hits
        .iter()
        .map(|(_, d)| d.observation_rank.unwrap())
        .collect();
    let mut sorted = ranks.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, vec![1, 2]);
}

#[tokio::test]
async fn a_session_without_a_page_contributes_nothing_and_the_raw_fallback_still_sees_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, proj) = scope(&store, "default", "app").await;
    let live = session(&store, ws, proj).await;
    observe(
        &store,
        ws,
        proj,
        live,
        "pangolin sighting in the live session",
    )
    .await;
    // A page at a different session's path, and an unrelated note, must not
    // absorb the vote.
    let unrelated = session(&store, ws, proj).await;
    session_page(&store, ws, proj, unrelated, "nothing relevant").await;
    store
        .writer
        .upsert_page(page(ws, proj, "notes/plain.md", "plain note"))
        .await
        .unwrap();

    assert!(search(&store, ws, proj, "pangolin").await.is_empty());
    let raw = store
        .reader
        .search_observations_for_project(ws, proj, "pangolin".into(), 5)
        .await
        .unwrap();
    assert_eq!(raw.len(), 1);
    assert_eq!(raw[0].session_id, live);
}

/// The vector stream has no similarity floor, so with one embedded page it
/// answers a query that page has nothing to do with. `lexical_match` is what
/// lets `memory_query` tell that apart from a real match and still run the
/// raw fallback for the live session above.
#[tokio::test]
async fn lexical_match_is_false_when_only_the_vector_stream_answers() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, proj) = scope(&store, "default", "app").await;
    let live = session(&store, ws, proj).await;
    observe(
        &store,
        ws,
        proj,
        live,
        "pangolin sighting in the live session",
    )
    .await;
    let plain = store
        .writer
        .upsert_page(page(ws, proj, "notes/plain.md", "plain note"))
        .await
        .unwrap();
    store
        .writer
        .store_embedding(
            plain,
            ai_memory_store::f32_vec_to_bytes(&[1.0, 0.05]),
            "test".into(),
            "test-model".into(),
            2,
        )
        .await
        .unwrap();
    let outcome = |query: &'static str| {
        let store = &store;
        async move {
            store
                .reader
                .hybrid_search_scopes_outcome(
                    &[(ws, proj)],
                    HybridSearch {
                        query,
                        vector: Some(VectorQuery {
                            vector: &[0.0, 1.0],
                            provider: "test",
                            model: "test-model",
                            dim: 2,
                        }),
                        limit: 10,
                        expiry_cutoff_us: None,
                        include_superseded: false,
                        explain: false,
                        owner: &ai_memory_core::OwnerFilter::Any,
                        filter: PageFilter::default(),
                    },
                )
                .await
                .unwrap()
        }
    };

    let vector_only = outcome("pangolin").await;
    let hit_paths: Vec<&str> = vector_only
        .hits
        .iter()
        .map(|(hit, _)| hit.path.as_str())
        .collect();
    assert_eq!(hit_paths, ["notes/plain.md"], "the vector stream answers");
    assert!(!vector_only.lexical_match);

    // Controls: page FTS, and an observation vote once the session has a
    // page, are each a match.
    assert!(outcome("plain").await.lexical_match);
    session_page(&store, ws, proj, live, "session summary").await;
    let voted = outcome("pangolin").await;
    assert!(voted.lexical_match);
    assert!(
        voted
            .hits
            .iter()
            .any(|(hit, _)| hit.path.as_str() == format!("sessions/{live}.md"))
    );
}

#[tokio::test]
async fn a_superseded_or_expired_session_page_takes_no_vote() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, proj) = scope(&store, "default", "app").await;
    let expired = session(&store, ws, proj).await;
    let mut expiring = page(ws, proj, &format!("sessions/{expired}.md"), "old session");
    expiring.expires_at = Some(jiff::Timestamp::from_second(1_000).unwrap());
    store.writer.upsert_page(expiring).await.unwrap();
    observe(&store, ws, proj, expired, "ocelot evidence").await;
    assert!(search(&store, ws, proj, "ocelot").await.is_empty());

    // A rewritten session page: only the latest version takes the vote.
    let rewritten = session(&store, ws, proj).await;
    session_page(&store, ws, proj, rewritten, "first draft").await;
    session_page(&store, ws, proj, rewritten, "second draft").await;
    observe(&store, ws, proj, rewritten, "axolotl evidence").await;
    let hits = search(&store, ws, proj, "axolotl").await;
    assert_eq!(hits.len(), 1);
    let latest = store
        .reader
        .latest_page_ids_by_paths(ws, proj, vec![format!("sessions/{rewritten}.md")])
        .await
        .unwrap();
    assert_eq!(vec![hits[0].0.id], latest);
}

/// Stream off must rank exactly as a store that never heard of the stream:
/// the observations present must not move a single hit, rank, or snippet,
/// and the explain JSON must carry no observation keys.
#[tokio::test]
async fn stream_off_ranks_byte_identically_to_a_store_without_observations() {
    async fn seeded(with_observations: bool) -> (tempfile::TempDir, Store, WorkspaceId, ProjectId) {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path()).unwrap();
        store.reader.set_retrieval_tuning(stream_off());
        let (ws, proj) = scope(&store, "default", "app").await;
        for (i, body) in [
            "kestrel nest under the bridge",
            "the kestrel came back",
            "nothing here",
        ]
        .iter()
        .enumerate()
        {
            store
                .writer
                .upsert_page(page(ws, proj, &format!("notes/n{i}.md"), body))
                .await
                .unwrap();
        }
        // Deterministic session ids so both stores share page paths.
        for i in 0..3_u128 {
            let id: SessionId = format!("00000000-0000-7000-8000-{:012x}", 0x1000 + i)
                .parse()
                .unwrap();
            store
                .writer
                .begin_session(NewSession {
                    occurred_at: None,
                    id,
                    workspace_id: ws,
                    project_id: proj,
                    agent_kind: AgentKind::ClaudeCode,
                    cwd: None,
                    actor_user: None,
                    origin: None,
                })
                .await
                .unwrap();
            session_page(&store, ws, proj, id, "session about birds").await;
            if with_observations {
                observe(&store, ws, proj, id, "a kestrel hovered over the field").await;
            }
        }
        (tmp, store, ws, proj)
    }
    let (_a, plain, pws, pproj) = seeded(false).await;
    let (_b, observed, ows, oproj) = seeded(true).await;
    for query in ["kestrel", "birds", "kestrel birds"] {
        let expected = search(&plain, pws, pproj, query).await;
        let actual = search(&observed, ows, oproj, query).await;
        let shape = |hits: &[(PageHit, SearchExplain)]| {
            hits.iter()
                .map(|(h, d)| {
                    let mut json = serde_json::to_value(d).unwrap();
                    // Ids differ between the two stores; everything else
                    // must match.
                    json.as_object_mut().unwrap().remove("graph_via");
                    (
                        h.path.as_str().to_string(),
                        h.title.clone(),
                        h.snippet.clone(),
                        h.rank.to_bits(),
                        json,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shape(&expected), shape(&actual), "query {query}");
        for (_, details) in &actual {
            let json = serde_json::to_string(details).unwrap();
            assert!(!json.contains("observation"), "{json}");
        }
    }
}

/// A session page with an empty title is still described by the page
/// descriptor fill, but that fill must not replace the observation excerpt
/// that explains why the page surfaced.
#[tokio::test]
async fn an_untitled_session_page_keeps_the_observation_excerpt() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, proj) = scope(&store, "default", "app").await;
    let sid = session(&store, ws, proj).await;
    let mut untitled = page(ws, proj, &format!("sessions/{sid}.md"), "short excerpt");
    untitled.title = String::new();
    store.writer.upsert_page(untitled).await.unwrap();
    observe(&store, ws, proj, sid, "the heron nested by the old mill").await;

    let hits = search(&store, ws, proj, "heron").await;
    assert_eq!(hits.len(), 1);
    assert!(
        ai_memory_store::highlighted_snippet_html(&hits[0].0.snippet)
            .contains("<mark>heron</mark>"),
        "{}",
        hits[0].0.snippet
    );
}

/// When page FTS matched the session page itself, the page keeps its own
/// snippet; the observation excerpt stands in only for a page FTS missed.
#[tokio::test]
async fn a_page_fts_matched_keeps_its_own_snippet() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, proj) = scope(&store, "default", "app").await;
    let sid = session(&store, ws, proj).await;
    session_page(&store, ws, proj, sid, "User mentioned the puffin colony.").await;
    // A denser page takes FTS rank 1, so the session page's observation vote
    // (stream rank 1) outranks its own FTS match (rank 2).
    store
        .writer
        .upsert_page(page(ws, proj, "notes/puffins.md", "puffin puffin puffin"))
        .await
        .unwrap();
    observe(
        &store,
        ws,
        proj,
        sid,
        "a long assistant reply about the puffin colony and its nesting season",
    )
    .await;

    let hits = search(&store, ws, proj, "puffin").await;
    let session_path = format!("sessions/{sid}.md");
    let (hit, details) = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == session_path)
        .unwrap();
    assert_eq!(details.fts_rank, Some(2));
    assert_eq!(details.observation_rank, Some(1));
    assert!(hit.snippet.contains("colony"), "{}", hit.snippet);
    assert!(!hit.snippet.contains("assistant"), "{}", hit.snippet);
}

/// A session page's title-only body shares topic words with many sessions;
/// the answer sits in one session's observation text. Page FTS must not let a
/// topic-word session outvote the session the observations rank first: a page
/// the stream voted for takes its lexical evidence from the stream alone.
#[tokio::test]
async fn a_topic_word_session_page_does_not_outvote_the_evidence_session() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, proj) = scope(&store, "default", "app").await;
    let evidence = session(&store, ws, proj).await;
    let topical = session(&store, ws, proj).await;
    session_page(
        &store,
        ws,
        proj,
        evidence,
        "User asked about weekend plans.",
    )
    .await;
    session_page(&store, ws, proj, topical, "Planning the telescope trip.").await;
    observe(
        &store,
        ws,
        proj,
        evidence,
        "I finally bought the quasar telescope from the camera shop downtown",
    )
    .await;
    observe(
        &store,
        ws,
        proj,
        topical,
        "which telescope should I pack for the trip",
    )
    .await;

    let hits = search(&store, ws, proj, "quasar telescope").await;
    let evidence_path = format!("sessions/{evidence}.md");
    let topical_path = format!("sessions/{topical}.md");
    assert_eq!(paths(&hits), vec![evidence_path, topical_path.clone()]);

    let (_, topical_details) = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == topical_path)
        .unwrap();
    // The page FTS match is still reported, but contributes nothing.
    assert_eq!(topical_details.fts_rank, Some(1));
    assert_eq!(topical_details.observation_rank, Some(2));
    assert!(topical_details.rrf.fts.abs() < 1e-12);
    assert!((topical_details.fused - topical_details.rrf.observation).abs() < 1e-12);
}

/// Control: a page the stream cannot vote for (anything but a session page)
/// keeps its page FTS contribution, and still outranks a session page the
/// stream ranked below it.
#[tokio::test]
async fn a_curated_page_keeps_its_page_fts_contribution() {
    let tmp = tempfile::tempdir().unwrap();
    let mut store = Store::open(tmp.path()).unwrap();
    store.reader.set_retrieval_tuning(stream_on());
    let (ws, proj) = scope(&store, "default", "app").await;
    let sid = session(&store, ws, proj).await;
    session_page(&store, ws, proj, sid, "User asked about weekend plans.").await;
    store
        .writer
        .upsert_page(page(
            ws,
            proj,
            "decisions/telescope.md",
            "We chose the quasar telescope for the observatory.",
        ))
        .await
        .unwrap();
    observe(&store, ws, proj, sid, "the quasar telescope arrived").await;

    let hits = search(&store, ws, proj, "quasar telescope").await;
    let (_, decision) = hits
        .iter()
        .find(|(h, _)| h.path.as_str() == "decisions/telescope.md")
        .unwrap();
    assert_eq!(decision.fts_rank, Some(1));
    assert!((decision.rrf.fts - 1.0 / (K + 1.0)).abs() < 1e-12);
    assert!(decision.rrf.observation.abs() < 1e-12);
}

/// The stream's rowid-span probe runs on every query, so it must stay a
/// min/max seek on the rowid-ordered `idx_observations_project`: filtering on
/// `(workspace_id, project_id)` let the planner choose the created_at-ordered
/// index and walk all of the project's entries. It must also return exactly
/// what the original two-column aggregate did, for every project in a store
/// whose projects interleave their writes.
#[tokio::test]
async fn the_rowid_span_probe_seeks_the_rowid_ordered_index_and_matches_the_aggregate() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let scopes = [
        scope(&store, "default", "alpha").await,
        scope(&store, "default", "beta").await,
        scope(&store, "other", "alpha").await,
    ];
    let mut sessions = Vec::new();
    for &(ws, proj) in &scopes {
        sessions.push(session(&store, ws, proj).await);
    }
    for round in 0..4 {
        for (i, &(ws, proj)) in scopes.iter().enumerate() {
            // The third project writes only in the middle rounds, so its span
            // sits strictly inside the others'.
            if i == 2 && !(1..=2).contains(&round) {
                continue;
            }
            observe(&store, ws, proj, sessions[i], &format!("note {round} {i}")).await;
        }
    }
    let (empty_ws, empty_proj) = scope(&store, "default", "empty").await;

    let conn = rusqlite::Connection::open(store.db_path()).unwrap();
    let plan: Vec<String> = conn
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            crate::reader::OBSERVATION_ROWID_SPAN_SQL
        ))
        .unwrap()
        .query_map([scopes[0].1.as_bytes()], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let plan = plan.join("\n");
    assert!(
        plan.contains("COVERING INDEX idx_observations_project (project_id=?)"),
        "span probe must seek the rowid-ordered index:\n{plan}"
    );
    assert!(
        !plan.contains("idx_observations_project_created") && !plan.contains("SCAN observations"),
        "span probe must not walk the created_at index or the table:\n{plan}"
    );

    let mut spans = Vec::new();
    for &(ws, proj) in scopes.iter().chain([(empty_ws, empty_proj)].iter()) {
        let legacy: (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT MIN(rowid), MAX(rowid) FROM observations \
                 WHERE workspace_id = ?1 AND project_id = ?2",
                rusqlite::params![ws.as_bytes(), proj.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let probe: (Option<i64>, Option<i64>) = conn
            .query_row(
                crate::reader::OBSERVATION_ROWID_SPAN_SQL,
                [proj.as_bytes()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(probe, legacy);
        spans.push(probe);
    }
    assert!(
        spans[..3]
            .iter()
            .all(|(lo, hi)| lo.is_some() && hi.is_some())
    );
    assert_ne!(
        spans[0], spans[1],
        "fixture projects must have distinct spans"
    );
    assert_eq!(
        spans[3],
        (None, None),
        "a project with no observations has no span"
    );
}
