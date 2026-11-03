//! Recall@5 eval harness.
//!
//! Loads a small hand-crafted corpus + a probe set, measures recall@5
//! against both the pure-FTS5 path and the hybrid (FTS5 + entity + graph RRF +
//! vector) path, and asserts a baseline. It also pins entity-only recall,
//! graph-neighbor expansion, and raw observation fallback. The point is *the
//! framework*: once the
//! harness is in CI, anyone can drop in real embeddings (set the
//! `AI_MEMORY_EMBEDDING_PROVIDER` env vars + plug in a real model) and
//! watch the numbers move. The synthetic embedder shipped for tests is
//! deterministic but not semantically aware, so for now hybrid ≈ FTS5
//! on most probes; we still log the numbers so regressions show up.
//!
//! The corpus mirrors the kind of pages an ai-memory wiki accrues over
//! a few sessions of building this project itself: session notes,
//! concept pages, decision logs.

use std::sync::Arc;

use ai_memory_core::{
    AgentKind, NewObservation, NewSession, ObservationKind, PagePath, Sanitized, Sanitizer,
    SessionId, Tier,
};
use ai_memory_llm::{Embedder, SyntheticEmbedder};
use ai_memory_store::{RetrievalTuning, Store};
use ai_memory_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

// Note: bodies and queries deliberately avoid hyphens, because the
// FTS5 query parser treats a bare `-` as the NOT operator and our
// tokenizer keeps `-` inside tokens (so `single-writer` is one token
// rather than two). Both rules together make hyphenated terms a
// landmine for free-text search; we sidestep by spelling things out.
const CORPUS: &[(&str, &str)] = &[
    (
        "notes/writer_actor.md",
        "The single writer SQLite actor owns one Connection and serialises every \
         mutation through a tokio mpsc bounded channel for backpressure",
    ),
    (
        "notes/karpathy_wiki.md",
        "Karpathy LLM Wiki compile knowledge into a durable artifact instead of \
         retrieving from raw documents at query time",
    ),
    (
        "notes/decay_policy.md",
        "Retention sweep evicts cold episodic pages whose retention falls below \
         threshold semantic concept pages compound forever pinned exempt",
    ),
    (
        "notes/handoff.md",
        "Cross agent handoff lets Codex pick up the working directory where \
         Claude Code left off via memory_handoff_begin and memory_handoff_accept",
    ),
    (
        "notes/backup.md",
        "SQLite online backup API snapshots the database while writes continue \
         used by ai memory backup to produce a consistent tarball",
    ),
    (
        "notes/identity_tuple.md",
        "Identity is the three tuple workspace project path on every domain row \
         baked into the schema from M1 to avoid retrofit pain",
    ),
    (
        "notes/mcp_transport.md",
        "MCP server speaks JSON RPC over stdio or streamable HTTP via the rmcp \
         crate protocol version pinned to 2024 11 05",
    ),
    (
        "notes/hooks_pipeline.md",
        "Claude Code lifecycle hooks fire and forget POST to the hook endpoint \
         on the ai memory server with sub second timeouts",
    ),
    (
        "notes/consolidate_fanout.md",
        "Multi page atomic fan out the consolidator emits a ConsolidatedBatch \
         that ai memory wiki applies in one SQL transaction",
    ),
    (
        "notes/hybrid_search.md",
        "Hybrid retrieval reciprocal rank fusion combines FTS5 keyword ranks with \
         cosine similarity over stored embeddings k equals 60",
    ),
];

/// Each probe is (free-text query, ground-truth wiki path).
const PROBES: &[(&str, &str)] = &[
    ("tokio mpsc backpressure", "notes/writer_actor.md"),
    ("compile knowledge artifact", "notes/karpathy_wiki.md"),
    ("retention sweep cold pages", "notes/decay_policy.md"),
    ("Codex Claude handoff", "notes/handoff.md"),
    ("SQLite online backup tarball", "notes/backup.md"),
    (
        "workspace project tuple identity",
        "notes/identity_tuple.md",
    ),
    ("rmcp JSON RPC stdio", "notes/mcp_transport.md"),
    ("lifecycle POST hook timeout", "notes/hooks_pipeline.md"),
    (
        "ConsolidatedBatch SQL transaction",
        "notes/consolidate_fanout.md",
    ),
    ("reciprocal rank fusion cosine", "notes/hybrid_search.md"),
];

const RECALL_FLOOR: f64 = 0.70;

#[tokio::test]
async fn portuguese_session_recall_is_opt_in_and_preserves_fact_queries() {
    let tmp = TempDir::new().expect("tempdir");
    let mut store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "eval", None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    // Identical searchable content isolates the existing session authority
    // penalty and its opt-in cancellation from differences in FTS matches.
    let body = "Na última sessão fizemos a busca SQLite. Ontem paramos na revisão. \
                A decisão anterior tratou do histórico. \
                Erro na sessão do usuário: a sessão expira antes de salvar. \
                A sessão passada para o middleware é nula; sessao passada como parametro.";
    for (path, tier, kind) in [
        ("notes/recall.md", Tier::Semantic, "note"),
        ("sessions/recall.md", Tier::Episodic, "session"),
    ] {
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path).expect("path"),
            frontmatter: serde_json::json!({"title": "Busca e sessões", "kind": kind}),
            body: body.into(),
            tier,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .expect("write page");
    }

    let probes = [
        ("o que fizemos na última sessão", true),
        ("o que fizemos na ultima sessao", true),
        ("o que fizemos na última sessao", true),
        ("o que fizemos na ultima sessão", true),
        ("ONDE PARAMOS ONTEM?", true),
        ("lembre a decisão anterior", true),
        ("lembre a decisao anterior", true),
        ("erro na sessão do usuário", false),
        ("erro na sessao do usuario", false),
        ("a sessão passada para o middleware é nula", false),
        ("sessao passada como parametro", false),
        ("sessão expira", false),
        ("sessao expira", false),
        ("antes de salvar", false),
        ("sessão", false),
        ("sessao", false),
        ("antes", false),
        ("penúltima sessão", false),
        ("decisão anteriormente tomada", false),
    ];
    for (query, historical) in probes {
        let mut baseline = Vec::new();
        for mode in 0..3 {
            store.reader.set_retrieval_tuning(if mode == 0 {
                RetrievalTuning::default()
            } else {
                RetrievalTuning {
                    session_recall_routing: mode == 2,
                    session_recall_bonus: 0.15,
                    ..RetrievalTuning::default()
                }
            });
            let hits = store
                .reader
                .hybrid_search_explained(
                    ws,
                    proj,
                    query.into(),
                    None,
                    String::new(),
                    String::new(),
                    0,
                    5,
                    None,
                    false,
                )
                .await
                .expect("search");
            assert_eq!(hits.len(), 2, "query={query:?}, mode={mode}");
            let ranking: Vec<_> = hits.iter().map(|(hit, _)| (hit.id, hit.rank)).collect();
            if mode == 0 {
                baseline = ranking;
                assert_eq!(hits[0].0.path.as_str(), "notes/recall.md", "{query}");
            } else if mode == 1 || !historical {
                assert_eq!(ranking, baseline, "query={query:?}, mode={mode}");
            } else {
                assert_eq!(hits[0].0.path.as_str(), "sessions/recall.md", "{query}");
                assert!(hits[0].1.intent_boost.expect("recall boost") > 1.0);
            }
            for (_, explain) in &hits {
                let expected = (mode == 2 && historical).then_some("session_recall");
                assert_eq!(explain.intent, expected, "query={query:?}, mode={mode}");
                if expected.is_none() {
                    assert_eq!(explain.intent_boost, None, "{query}");
                }
            }
        }
    }
    eprintln!(
        "portuguese_session_recall: {} probes checked off/on",
        probes.len()
    );
}

#[tokio::test]
async fn recall_at_5_baseline() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "eval", None)
        .await
        .expect("proj");
    let embedder: Arc<dyn Embedder> = Arc::new(SyntheticEmbedder::new(128));
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .expect("wiki")
        .with_embedder(embedder.clone());

    for (path, body) in CORPUS {
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new(path.to_string()).expect("path"),
            frontmatter: serde_json::json!({"title": path}),
            body: body.to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .expect("write page");
    }

    let fts_recall = measure_recall(&store, ws, proj, None).await;
    let hybrid_recall = measure_recall(&store, ws, proj, Some(embedder.clone())).await;

    // Recall numbers are persisted to stderr so CI logs always show
    // them, even when the run is green. Watch these drift over time.
    eprintln!("recall_eval: FTS5={fts_recall:.3}, hybrid={hybrid_recall:.3}");

    assert!(
        fts_recall >= RECALL_FLOOR,
        "pure-FTS5 recall@5 below baseline: got {fts_recall:.3} (floor {RECALL_FLOOR})"
    );
    assert!(
        hybrid_recall >= RECALL_FLOOR,
        "hybrid recall@5 below baseline: got {hybrid_recall:.3} (floor {RECALL_FLOOR})"
    );

    // RRF should not *hurt* recall vs the FTS5-only path on this
    // corpus. We don't assert hybrid > FTS5 because the synthetic
    // embedder doesn't add real semantic signal — that's a feature of
    // the test, not a bug.
    assert!(
        hybrid_recall + 1e-6 >= fts_recall,
        "hybrid should never regress vs FTS5: FTS5={fts_recall:.3} hybrid={hybrid_recall:.3}",
    );
}

#[tokio::test]
async fn graph_neighbor_expansion_recovers_linked_page() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "eval", None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");

    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("notes/hidden_target.md").expect("path"),
        frontmatter: serde_json::json!({"title": "Hidden target"}),
        body: "This page contains implementation detail but not the seed term.".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .expect("write target");
    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("notes/source.md").expect("path"),
        frontmatter: serde_json::json!({"title": "Source"}),
        body: "graphseed points to [[notes/hidden_target]].".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .expect("write source");

    let fts_hits = store
        .reader
        .search_pages_for_project(ws, proj, "graphseed".into(), 5, None)
        .await
        .expect("fts search");
    assert!(
        !fts_hits
            .iter()
            .any(|hit| hit.path.as_str() == "notes/hidden_target.md"),
        "pure FTS should not find the target without the seed term"
    );

    let hybrid_hits = store
        .reader
        .hybrid_search(
            ws,
            proj,
            "graphseed".into(),
            None,
            String::new(),
            String::new(),
            0,
            5,
            None,
            false,
        )
        .await
        .expect("hybrid search");
    assert!(
        hybrid_hits
            .iter()
            .any(|hit| hit.path.as_str() == "notes/hidden_target.md"),
        "graph expansion should include the linked target"
    );
}

/// The entity stream recovers a probe that BOTH pure FTS and the graph
/// stream miss: the page's body never carries the query's wording, and it
/// has no links. The only bridge is the frontmatter `entities:` list the
/// consolidator writes.
#[tokio::test]
async fn entity_stream_recovers_a_probe_fts_and_graph_both_miss() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "eval", None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");

    wiki.write_page(WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new("decisions/queue-choice.md").expect("path"),
        // Body deliberately avoids the probe term and carries no links.
        frontmatter: serde_json::json!({
            "title": "Queue choice",
            "entities": ["nats jetstream", "delivery guarantees"],
        }),
        body: "We picked the streaming broker for at-least-once delivery.".into(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
        evidence: Vec::new(),
    })
    .await
    .expect("write page");

    let probe = "jetstream";
    let fts_hits = store
        .reader
        .search_pages_for_project(ws, proj, probe.into(), 5, None)
        .await
        .expect("fts search");
    assert!(
        fts_hits.is_empty(),
        "pure FTS must miss: the body never says {probe}"
    );

    let hybrid_hits = store
        .reader
        .hybrid_search(
            ws,
            proj,
            probe.into(),
            None,
            String::new(),
            String::new(),
            0,
            5,
            None,
            false,
        )
        .await
        .expect("hybrid search");
    assert!(
        hybrid_hits
            .iter()
            .any(|hit| hit.path.as_str() == "decisions/queue-choice.md"),
        "the entity stream should recover the page: {:?}",
        hybrid_hits
            .iter()
            .map(|h| h.path.as_str())
            .collect::<Vec<_>>(),
    );
}

#[tokio::test]
async fn raw_observation_fallback_recovers_detail_when_wiki_misses() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, "eval", None)
        .await
        .expect("proj");
    let session_id = SessionId::new();

    store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session_id,
            workspace_id: ws,
            project_id: proj,
            agent_kind: AgentKind::OpenCode,
            cwd: None,
            actor_user: None,
        })
        .await
        .expect("begin session");
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
                title: "raw prompt".into(),
                body: "raw fallback detail mentions capybara exactly once".into(),
                importance: 5,
                file_path: None,
            },
            &Sanitizer::builtin(),
        ))
        .await
        .expect("insert observation");

    let page_hits = store
        .reader
        .search_pages_for_project(ws, proj, "capybara".into(), 5, None)
        .await
        .expect("page search");
    assert!(page_hits.is_empty(), "compiled wiki should miss");

    let raw_hits = store
        .reader
        .search_observations_for_project(ws, proj, "capybara".into(), 5)
        .await
        .expect("raw search");
    assert_eq!(raw_hits.len(), 1);
    assert_eq!(raw_hits[0].session_id, session_id);
    assert!(
        ai_memory_store::highlighted_snippet_html(&raw_hits[0].snippet)
            .contains("<mark>capybara</mark>")
    );
}

/// Cross-project profile recall (`docs/cross-project-profile.md`): a habit the
/// user stated in two projects becomes a profile entry that a task-level
/// query finds in the profile scope, where every other project's
/// `memory_query` unions it. A choice only one project made stays out.
#[tokio::test]
async fn profile_recall_across_projects() {
    use ai_memory_consolidate::profile::{ProfilePassConfig, run_profile_pass};

    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("open store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .expect("wiki")
        .with_store_reader(store.reader.clone());
    for (project, prompt) in [
        ("alpha", "I prefer tabs over spaces."),
        // Reworded: the same text in two projects at the same moment is one
        // message fanned out, not a habit (#1148).
        ("beta", "I prefer tabs over spaces here too."),
        (
            "alpha",
            "Use the legacy webpack builder instead of vite in this repo.",
        ),
    ] {
        let proj = store
            .writer
            .get_or_create_project(ws, project, None)
            .await
            .expect("proj");
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                occurred_at: None,
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::ClaudeCode,
                cwd: None,
                actor_user: None,
            })
            .await
            .expect("begin session");
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
                    body: prompt.into(),
                    importance: 5,
                    file_path: None,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .expect("insert observation");
    }
    let config = ProfilePassConfig {
        settings: ai_memory_core::profile::ProfileSettings::default(),
        distinguishes_operators: false,
    };
    run_profile_pass(&store.reader, &store.writer, &wiki, None, &config)
        .await
        .expect("profile pass");

    let global = ai_memory_store::lookup_global_scope(&store.reader)
        .await
        .expect("lookup")
        .expect("the profile scope exists");
    let hits = store
        .reader
        .search_pages_for_project(
            global.workspace_id,
            global.project_id,
            "tabs spaces".into(),
            5,
            None,
        )
        .await
        .expect("profile search");
    assert!(
        hits.iter().any(|h| h.path.as_str().starts_with("profile/")),
        "the converged habit is recallable from the profile: {hits:?}"
    );
    let local = store
        .reader
        .search_pages_for_project(
            global.workspace_id,
            global.project_id,
            "webpack builder".into(),
            5,
            None,
        )
        .await
        .expect("profile search");
    assert!(
        local.is_empty(),
        "a one-project exception never reaches the profile: {local:?}"
    );
}

async fn measure_recall(
    store: &Store,
    ws: ai_memory_core::WorkspaceId,
    proj: ai_memory_core::ProjectId,
    embedder: Option<Arc<dyn Embedder>>,
) -> f64 {
    let mut hits = 0_usize;
    for (query, expected) in PROBES {
        let results = if let Some(emb) = &embedder {
            let qv = emb.embed_query(query).await.expect("embed query");
            store
                .reader
                .hybrid_search(
                    ws,
                    proj,
                    (*query).to_string(),
                    Some(qv),
                    emb.provider().to_string(),
                    emb.model().to_string(),
                    emb.dim(),
                    5,
                    None,
                    false,
                )
                .await
                .expect("hybrid search")
        } else {
            store
                .reader
                .search_pages((*query).to_string(), 5, None)
                .await
                .expect("search")
        };
        let found = results.iter().any(|r| r.path.as_str() == *expected);
        if found {
            hits += 1;
        } else {
            eprintln!(
                "  miss: query={query:?} expected={expected:?} top5={:?}",
                results.iter().map(|r| r.path.as_str()).collect::<Vec<_>>()
            );
        }
    }
    #[allow(clippy::cast_precision_loss)]
    {
        hits as f64 / PROBES.len() as f64
    }
}
