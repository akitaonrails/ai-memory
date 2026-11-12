//! The recall trace on `/web/search` and the scope rules of project search.
//!
//! A project search runs `memory_query`'s hybrid ranker (the order parity is
//! proven in `ai-memory-mcp/tests/suite/web_search_parity.rs`). Here: what
//! `explain=1` renders, and that a project search, with or without the trace,
//! follows `memory_query`'s scope rules — a restricted project refused to a
//! viewer without a grant, an unknown project not found and never created,
//! partial scope rejected, and no other project's pages in the result or the
//! trace.

use std::sync::{Arc, Mutex};

use ai_memory_core::{
    ActorContext, AuthorizedViewer, NewUser, PagePath, ProjectId, Tier, UserId, UserRole,
    WorkspaceId,
};
use ai_memory_llm::{
    Embedder, RerankCandidate, RerankScore, RerankStage, Reranker, SyntheticEmbedder,
};
use ai_memory_store::{GrantLevel, Store};
use ai_memory_web::{Recall, WebMountSpec, split_web_routers};
use ai_memory_wiki::{Wiki, WritePageRequest};
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tempfile::TempDir;
use tower::ServiceExt;

struct Fixture {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
    alpha: ProjectId,
}

fn embedder() -> Arc<dyn Embedder> {
    Arc::new(SyntheticEmbedder::new(16))
}

fn req(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> WritePageRequest {
    WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        frontmatter: serde_json::json!({"kind": "fact"}),
        body: body.to_owned(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ActorContext::anonymous(),
        evidence: Vec::new(),
    }
}

/// `alpha` holds the target pages (one links to another, so the graph
/// stream has something to follow); `beta` holds a page that matches the
/// same query better and must never surface from an `alpha` search.
async fn fixture(restricted: bool) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    if restricted {
        store
            .writer
            .set_new_project_mode(ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
    }
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let alpha = store
        .writer
        .get_or_create_project(ws, "alpha", None)
        .await
        .unwrap();
    let beta = store
        .writer
        .get_or_create_project(ws, "beta", None)
        .await
        .unwrap();
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_embedder(embedder());
    for (path, body) in [
        (
            "notes/rotation.md",
            "# Token rotation\n\nRotate the signing token weekly. See [[notes/vault]].",
        ),
        (
            "notes/vault.md",
            "# Vault\n\nKeys live in the vault and are confidential.",
        ),
        ("notes/deploy.md", "# Deploy\n\nThe pipeline swaps traffic."),
    ] {
        wiki.write_page(req(ws, alpha, path, body)).await.unwrap();
    }
    wiki.write_page(req(
        ws,
        beta,
        "notes/beta-rotation.md",
        "# Beta token rotation\n\nBETA-ONLY-MARKER token rotation token rotation.",
    ))
    .await
    .unwrap();
    Fixture {
        _tmp: tmp,
        store,
        wiki: wiki.without_embedder(),
        alpha,
    }
}

fn web(f: &Fixture, recall: Recall) -> axum::Router {
    split_web_routers(
        true,
        f.store.reader.clone(),
        f.wiki.clone(),
        WebMountSpec {
            enable_api: true,
            web_ui_dir: None,
            cors_origins: &[],
            web_slug: "/web",
            base_href: "/web/",
            base_path: "",
            trusted_proxy_identity: false,
            recall,
            runtime: None,
            health: None,
        },
    )
    .unwrap()
    .protected
}

async fn get(app: &axum::Router, uri: &str, viewer: Option<UserId>) -> (StatusCode, String) {
    let mut request = Request::builder().uri(uri);
    if let Some(viewer) = viewer {
        request = request.extension(AuthorizedViewer(viewer));
    }
    let resp = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn human(store: &Store, name: &str) -> UserId {
    store
        .writer
        .create_human_user(
            NewUser {
                username: name.into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn explain_renders_stream_ranks_rrf_authority_and_timings() {
    let f = fixture(false).await;
    let app = web(
        &f,
        Recall {
            embedder: Some(embedder()),
            rerank: None,
        },
    );
    let (status, body) = get(
        &app,
        "/web/search?q=token%20rotation&workspace=default&project=alpha&explain=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Recall trace"), "{body}");
    assert!(body.contains("Hybrid ranker"), "{body}");
    // The streams that ran, the embedder that fed the vector stream.
    assert!(
        body.contains("fts, entity, vector, graph"),
        "streams_active missing: {body}"
    );
    assert!(
        body.contains("synthetic / bag-of-words-v1 (dim 16)"),
        "{body}"
    );
    // Per-stream timings, one row each, vector included.
    for stream in ["fts", "entity", "vector", "graph"] {
        assert!(
            body.contains(&format!(
                "<tr><td class=\"pr-4 font-mono\">{stream}</td><td class=\"text-right font-mono\">"
            )),
            "no {stream} timing row: {body}"
        );
    }
    // Per-hit trace: rank, fused, authority, and the stream table.
    assert!(body.contains("data-trace"), "{body}");
    assert!(
        body.contains("· fused <span class=\"font-mono\">"),
        "{body}"
    );
    assert!(
        body.contains("· authority <span class=\"font-mono\">"),
        "{body}"
    );
    assert!(body.contains(">RRF</th>"), "{body}");
    // The page the FTS stream ranked first carries an FTS rank of 1.
    assert!(
        body.contains(
            "<td class=\"pr-4 font-mono\">fts</td><td class=\"pr-4 text-right font-mono\">1</td>"
        ),
        "{body}"
    );
    // Nothing from beta: neither its path nor its body, though it matches
    // the query better.
    assert!(!body.contains("beta-rotation"), "{body}");
    assert!(!body.contains("BETA-ONLY-MARKER"), "{body}");

    // Without explain the same search renders no trace.
    let (_, plain) = get(
        &app,
        "/web/search?q=token%20rotation&workspace=default&project=alpha",
        None,
    )
    .await;
    assert!(plain.contains("notes/rotation.md"), "{plain}");
    assert!(!plain.contains("data-trace"), "{plain}");
    assert!(!plain.contains("Recall trace"), "{plain}");
}

#[tokio::test]
async fn explain_without_an_embedder_reports_no_vector_stream() {
    let f = fixture(false).await;
    let app = web(&f, Recall::default());
    let (status, body) = get(
        &app,
        "/web/search?q=token%20rotation&workspace=default&project=alpha&explain=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.contains("Embedder: <span class=\"font-mono\">none</span>"),
        "{body}"
    );
    assert!(body.contains("fts, entity, graph"), "{body}");
    assert!(!body.contains("fts, entity, vector"), "{body}");
    assert!(body.contains("notes/rotation.md"), "{body}");
}

#[tokio::test]
async fn cross_project_explain_is_fts_only_and_carries_no_per_hit_trace() {
    let f = fixture(false).await;
    let app = web(
        &f,
        Recall {
            embedder: Some(embedder()),
            rerank: None,
        },
    );
    let (status, body) = get(&app, "/web/search?q=rotation&explain=1", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("Cross-project search is FTS5 only"), "{body}");
    assert!(!body.contains("data-trace"), "{body}");
    // Both projects' pages, as a global search should.
    assert!(body.contains("notes/rotation.md"), "{body}");
    assert!(body.contains("notes/beta-rotation.md"), "{body}");
}

#[tokio::test]
async fn project_search_refuses_a_restricted_project_without_a_grant() {
    let f = fixture(true).await;
    let alice = human(&f.store, "alice").await;
    let bob = human(&f.store, "bob").await;
    f.store
        .writer
        .grant_memory(alice, f.alpha, GrantLevel::Read, None)
        .await
        .unwrap();
    let app = web(
        &f,
        Recall {
            embedder: Some(embedder()),
            rerank: None,
        },
    );
    for uri in [
        "/web/search?q=token%20rotation&workspace=default&project=alpha&explain=1",
        "/web/search?q=token%20rotation&workspace=default&project=alpha",
        "/api/v1/search?q=token%20rotation&workspace=default&project=alpha",
    ] {
        let (status, body) = get(&app, uri, Some(bob)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
        assert!(body.contains("not authorized for alpha"), "{uri}: {body}");
        for leaked in ["notes/rotation.md", "confidential", "data-trace", "fused"] {
            assert!(!body.contains(leaked), "{uri} leaked {leaked}: {body}");
        }
        // Controls: the granted reader and an unrestricted viewer find it.
        let (status, body) = get(&app, uri, Some(alice)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(body.contains("notes/rotation.md"), "{uri}: {body}");
        let (status, body) = get(&app, uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(body.contains("notes/rotation.md"), "{uri}: {body}");
    }

    // The same refusal through `scopes`, when one of them is foreign.
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/search")
        .header("content-type", "application/json")
        .extension(AuthorizedViewer(bob))
        .body(Body::from(
            serde_json::json!({
                "q": "token rotation",
                "scopes": [
                    {"workspace": "default", "project": "beta"},
                    {"workspace": "default", "project": "alpha"}
                ]
            })
            .to_string(),
        ))
        .unwrap();
    let resp = app.clone().oneshot(request).await.unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains("notes/rotation.md"), "{body}");
}

#[tokio::test]
async fn project_search_fails_closed_on_unknown_and_partial_scope() {
    let f = fixture(false).await;
    let app = web(&f, Recall::default());
    let before = f
        .store
        .reader
        .list_projects_with_stats(None)
        .await
        .unwrap()
        .len();

    let (status, body) = get(
        &app,
        "/web/search?q=token&workspace=default&project=nope&explain=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = get(
        &app,
        "/web/search?q=token&workspace=elsewhere&project=alpha&explain=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(!body.contains("notes/rotation.md"), "{body}");
    // A read never creates the scope it was asked about.
    assert_eq!(
        f.store
            .reader
            .list_projects_with_stats(None)
            .await
            .unwrap()
            .len(),
        before
    );

    for uri in [
        "/web/search?q=token&workspace=default",
        "/web/search?q=token&project=alpha",
    ] {
        let (status, _) = get(&app, uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn search_scope_selector_lists_only_readable_projects() {
    let f = fixture(true).await;
    let alice = human(&f.store, "alice").await;
    let bob = human(&f.store, "bob").await;
    f.store
        .writer
        .grant_memory(alice, f.alpha, GrantLevel::Read, None)
        .await
        .unwrap();
    let app = web(&f, Recall::default());
    let uri = "/web/search?q=token";

    let (status, body) = get(&app, uri, Some(bob)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for hidden in ["default/alpha", "default/beta"] {
        assert!(!body.contains(hidden), "bob sees {hidden}: {body}");
    }
    // A hand-typed scope is refused, not listed.
    let (status, body) = get(&app, "/web/search?q=token&scope=default/alpha", Some(bob)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains("notes/rotation.md"), "{body}");

    // Controls: the granted reader sees alpha only, an unrestricted viewer both.
    let (_, body) = get(&app, uri, Some(alice)).await;
    assert!(body.contains(r#"<option value="default/alpha">"#), "{body}");
    assert!(!body.contains("default/beta"), "{body}");
    let (_, body) = get(&app, uri, None).await;
    assert!(body.contains(r#"<option value="default/alpha">"#), "{body}");
    assert!(body.contains(r#"<option value="default/beta">"#), "{body}");
}

/// The recent-pages list a term-less search shows is as scoped as a search: a
/// restricted project's pages never reach a viewer without a grant.
#[tokio::test]
async fn search_without_a_term_lists_only_readable_projects_pages() {
    let f = fixture(true).await;
    let alice = human(&f.store, "alice").await;
    let bob = human(&f.store, "bob").await;
    f.store
        .writer
        .grant_memory(alice, f.alpha, GrantLevel::Read, None)
        .await
        .unwrap();
    let app = web(&f, Recall::default());

    let (status, body) = get(&app, "/web/search", Some(bob)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for hidden in ["notes/rotation.md", "notes/beta-rotation.md"] {
        assert!(!body.contains(hidden), "bob sees {hidden}: {body}");
    }

    // Controls: the granted reader sees alpha's pages only, an unrestricted
    // viewer both.
    let (_, body) = get(&app, "/web/search", Some(alice)).await;
    assert!(body.contains("notes/rotation.md"), "{body}");
    assert!(!body.contains("notes/beta-rotation.md"), "{body}");
    let (_, body) = get(&app, "/web/search", None).await;
    assert!(body.contains("notes/rotation.md"), "{body}");
    assert!(body.contains("notes/beta-rotation.md"), "{body}");
}

/// Web and `/api/v1` project search apply `memory_query`'s owner rule to
/// observation text: another operator's owned session still lifts its shared
/// session page, but the hit shows the page's own snippet and the trace no
/// observation id. Control: the session's owner sees the excerpt.
#[tokio::test]
async fn project_search_shows_observation_excerpts_only_to_the_sessions_owner() {
    use ai_memory_core::{AgentKind, NewObservation, NewSession, ObservationKind, SessionId};

    let f = fixture(false).await;
    let ws = f
        .store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let session_id = SessionId::new();
    f.store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session_id,
            workspace_id: ws,
            project_id: f.alpha,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: Some(ai_memory_core::IdentityKey::User("alice".into()).storage_key()),
            origin: None,
        })
        .await
        .unwrap();
    let observation_id = f
        .store
        .writer
        .insert_observation(ai_memory_core::Sanitized::new(
            NewObservation {
                occurred_at: None,
                session_id,
                workspace_id: ws,
                project_id: f.alpha,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "quokka plutonium shipment notes".into(),
                importance: 5,
                file_path: None,
            },
            &ai_memory_core::Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    f.store
        .writer
        .upsert_page(ai_memory_core::NewPage {
            workspace_id: ws,
            project_id: f.alpha,
            path: PagePath::new(format!("sessions/{session_id}.md")).unwrap(),
            title: "Alice session".into(),
            body: "Alice reviewed the release.".into(),
            tier: Tier::Episodic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    let app = web(&f, Recall::default());
    let as_operator = |user: &'static str, uri: &'static str| {
        let app = app.clone();
        async move {
            let request = Request::builder()
                .uri(uri)
                .extension(ActorContext {
                    user: Some(user.into()),
                    ..ActorContext::default()
                })
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(request).await.unwrap();
            let status = resp.status();
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            (status, String::from_utf8_lossy(&body).into_owned())
        }
    };
    let session_path = format!("sessions/{session_id}.md");
    for uri in [
        "/web/search?q=quokka&workspace=default&project=alpha&explain=1",
        "/api/v1/search?q=quokka&workspace=default&project=alpha",
    ] {
        let (status, body) = as_operator("bob", uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(body.contains(&session_path), "{uri}: vote kept: {body}");
        assert!(!body.contains("plutonium"), "{uri}: excerpt leaked: {body}");
        assert!(
            !body.contains(&observation_id.to_string()),
            "{uri}: observation id leaked: {body}"
        );
        let (status, body) = as_operator("alice", uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(body.contains("plutonium"), "{uri}: owner's excerpt: {body}");
    }
}

/// A procedural `procedures/rotate.md` in `alpha`, matching the same query as
/// the fixture's `fact`/`semantic` pages.
async fn add_procedure(f: &Fixture) {
    let ws = f
        .store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    f.wiki
        .write_page(WritePageRequest {
            frontmatter: serde_json::json!({"kind": "procedure"}),
            tier: Tier::Procedural,
            ..req(
                ws,
                f.alpha,
                "procedures/rotate.md",
                "# Rotate\n\nRun the token rotation script every week.",
            )
        })
        .await
        .unwrap();
}

fn hit_paths(body: &str) -> Vec<String> {
    body.split("data-hit-path=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn search_filters_a_project_by_kind_and_tier_keeping_rank_order() {
    let f = fixture(false).await;
    add_procedure(&f).await;
    let app = web(&f, Recall::default());
    let base = "/web/search?q=token%20rotation&workspace=default&project=alpha";

    let (status, body) = get(&app, base, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let all = hit_paths(&body);
    assert!(all.contains(&"procedures/rotate.md".to_owned()), "{all:?}");
    assert!(all.contains(&"notes/rotation.md".to_owned()), "{all:?}");

    let (_, body) = get(&app, &format!("{base}&kind=procedure"), None).await;
    assert_eq!(hit_paths(&body), ["procedures/rotate.md"], "{body}");
    let (_, body) = get(&app, &format!("{base}&tier=procedural"), None).await;
    assert_eq!(hit_paths(&body), ["procedures/rotate.md"], "{body}");
    // The tier ignores case, as the kind does.
    let (status, body) = get(&app, &format!("{base}&tier=Procedural"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(hit_paths(&body), ["procedures/rotate.md"], "{body}");

    // The survivors of `kind=fact` are the unfiltered list minus the
    // procedure, in the same order: dropping one page from every stream
    // leaves the others' relative ranks alone here.
    let expected: Vec<_> = all
        .iter()
        .filter(|p| *p != "procedures/rotate.md")
        .cloned()
        .collect();
    let (_, body) = get(&app, &format!("{base}&kind=fact&tier=semantic"), None).await;
    assert_eq!(hit_paths(&body), expected, "{body}");

    // Filters combine with AND, and an empty result says why.
    let (status, body) = get(&app, &format!("{base}&kind=procedure&tier=semantic"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(hit_paths(&body).is_empty(), "{body}");
    assert!(body.contains("with these filters"), "{body}");
    // "Search all projects" keeps the filters, so it does not read as if
    // the filter matched elsewhere.
    assert!(
        body.contains(r#"href="search?q=token%20rotation&amp;kind=procedure&amp;tier=semantic""#),
        "{body}"
    );

    // The selects keep the current choice.
    let (_, body) = get(
        &app,
        &format!("{base}&kind=procedure&tier=procedural"),
        None,
    )
    .await;
    assert!(
        body.contains(r#"<option value="procedure" selected>"#),
        "{body}"
    );
    assert!(
        body.contains(r#"<option value="procedural" selected>"#),
        "{body}"
    );
}

#[tokio::test]
async fn cross_project_search_filters_by_kind_and_rejects_an_unknown_tier() {
    let f = fixture(false).await;
    add_procedure(&f).await;
    let app = web(&f, Recall::default());

    let (_, body) = get(&app, "/web/search?q=token%20rotation&kind=procedure", None).await;
    assert_eq!(hit_paths(&body), ["procedures/rotate.md"], "{body}");
    // Control: without the filter beta's page is there too.
    let (_, body) = get(&app, "/web/search?q=token%20rotation", None).await;
    assert!(body.contains("notes/beta-rotation.md"), "{body}");

    for uri in [
        "/web/search?q=token&tier=bogus",
        "/web/search?q=token&workspace=default&project=alpha&tier=bogus",
    ] {
        let (status, _) = get(&app, uri, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}

/// A filter only narrows: it never reaches a project the viewer may not read.
#[tokio::test]
async fn kind_filter_does_not_widen_what_a_viewer_can_read() {
    let f = fixture(true).await;
    add_procedure(&f).await;
    let alice = human(&f.store, "alice").await;
    let bob = human(&f.store, "bob").await;
    f.store
        .writer
        .grant_memory(alice, f.alpha, GrantLevel::Read, None)
        .await
        .unwrap();
    let app = web(&f, Recall::default());

    for uri in [
        "/web/search?q=token%20rotation&kind=fact",
        "/web/search?q=token%20rotation&kind=procedure&tier=procedural",
    ] {
        let (status, body) = get(&app, uri, Some(bob)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(hit_paths(&body).is_empty(), "bob sees hits: {body}");
    }
    let (status, body) = get(
        &app,
        "/web/search?q=token%20rotation&scope=default/alpha&kind=procedure",
        Some(bob),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(!body.contains("procedures/rotate.md"), "{body}");

    // Controls: alice reads alpha, not beta; an unrestricted viewer reads both.
    let (_, body) = get(
        &app,
        "/web/search?q=token%20rotation&kind=fact",
        Some(alice),
    )
    .await;
    assert!(body.contains("notes/rotation.md"), "{body}");
    assert!(!body.contains("notes/beta-rotation.md"), "{body}");
    let (_, body) = get(&app, "/web/search?q=token%20rotation&kind=fact", None).await;
    assert!(body.contains("notes/beta-rotation.md"), "{body}");
}

#[tokio::test]
async fn kind_filter_is_case_insensitive_and_flags_a_nonstandard_kind() {
    let f = fixture(false).await;
    add_procedure(&f).await;
    let app = web(&f, Recall::default());
    let base = "/web/search?q=token%20rotation&workspace=default&project=alpha";

    let (_, body) = get(&app, &format!("{base}&kind=Procedure"), None).await;
    assert_eq!(hit_paths(&body), ["procedures/rotate.md"], "{body}");
    assert!(!body.contains("is not a standard page kind"), "{body}");

    // A typo matches nothing and says so, instead of looking like no results.
    let (status, body) = get(&app, &format!("{base}&kind=procedures"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(hit_paths(&body).is_empty(), "{body}");
    assert!(body.contains("is not a standard page kind"), "{body}");
}

/// Scores every candidate equally and remembers which pages it was shown.
#[derive(Default)]
struct RecordingReranker {
    seen: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Reranker for RecordingReranker {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn model(&self) -> &str {
        "test"
    }

    async fn rerank(
        &self,
        _query: &str,
        candidates: &[RerankCandidate],
    ) -> ai_memory_llm::LlmResult<Vec<RerankScore>> {
        self.seen
            .lock()
            .unwrap()
            .extend(candidates.iter().map(|c| c.title.clone()));
        Ok(candidates
            .iter()
            .map(|c| RerankScore {
                id: c.id.clone(),
                relevance: 0.5,
            })
            .collect())
    }
}

/// The streams filter before the reranker: pages the filter drops are never
/// sent to the (paid, slow) reranker.
#[tokio::test]
async fn reranker_is_shown_only_the_pages_the_filter_keeps() {
    let f = fixture(false).await;
    add_procedure(&f).await;
    // The stage leaves a single candidate alone, so the filter needs two.
    let ws = f
        .store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    f.wiki
        .write_page(WritePageRequest {
            frontmatter: serde_json::json!({"kind": "procedure"}),
            tier: Tier::Procedural,
            ..req(
                ws,
                f.alpha,
                "procedures/audit.md",
                "# Audit\n\nAudit every token rotation after the script ran.",
            )
        })
        .await
        .unwrap();
    let reranker = Arc::new(RecordingReranker::default());
    let recall = Recall {
        embedder: None,
        rerank: Some(RerankStage::new(reranker.clone())),
    };
    let app = web(&f, recall);
    let base = "/web/search?q=token%20rotation&workspace=default&project=alpha";

    // Control: unfiltered, the reranker sees more than the procedure.
    let (status, body) = get(&app, base, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let unfiltered = std::mem::take(&mut *reranker.seen.lock().unwrap());
    assert!(unfiltered.len() > 2, "{unfiltered:?}");

    let (status, body) = get(&app, &format!("{base}&kind=procedure"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut shown = hit_paths(&body);
    shown.sort();
    assert_eq!(
        shown,
        ["procedures/audit.md", "procedures/rotate.md"],
        "{body}"
    );
    let filtered = reranker.seen.lock().unwrap().clone();
    assert_eq!(filtered.len(), 2, "{filtered:?}");
}

/// A rare kind that the unfiltered result ranks out of view is still found:
/// the FTS stream filters in SQL, so 60 better-ranked facts, more than its
/// candidate window, cannot push the gotcha out.
#[tokio::test]
async fn kind_filter_finds_a_rare_kind_the_unfiltered_page_ranks_out() {
    let f = fixture(false).await;
    let ws = f
        .store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    for n in 0..60 {
        f.wiki
            .write_page(req(
                ws,
                f.alpha,
                &format!("notes/crowd-{n}.md"),
                "# Crowd\n\ncrowdword crowdword crowdword crowdword crowdword.",
            ))
            .await
            .unwrap();
    }
    f.wiki
        .write_page(WritePageRequest {
            frontmatter: serde_json::json!({"kind": "gotcha"}),
            ..req(
                ws,
                f.alpha,
                "gotchas/quiet.md",
                "# Quiet\n\nOne mention of crowdword among many other unrelated words \
                 about cats, rivers, mountains, bridges and old maps.",
            )
        })
        .await
        .unwrap();
    let app = web(&f, Recall::default());
    let base = "/web/search?q=crowdword&workspace=default&project=alpha&limit=5";

    let (_, body) = get(&app, base, None).await;
    assert!(
        !hit_paths(&body).contains(&"gotchas/quiet.md".to_owned()),
        "control: the gotcha must be ranked out of the first page: {body}"
    );
    let (_, body) = get(&app, &format!("{base}&kind=gotcha"), None).await;
    assert_eq!(hit_paths(&body), ["gotchas/quiet.md"], "{body}");
}

/// Every stream applies the filter itself. Each non-procedure page below is
/// reached by its own stream (page FTS, entity, vector, both directions of
/// graph from the procedure, observation votes), so dropping the predicate from any one of
/// them lets that page into a `kind=procedure` result. Control: unfiltered,
/// all of them are hits.
#[tokio::test]
async fn kind_and_tier_filters_hold_in_every_stream() {
    use ai_memory_core::{AgentKind, NewObservation, NewSession, ObservationKind, SessionId};

    let f = fixture(false).await;
    let ws = f
        .store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    // Written with the embedder, so the vector stream reaches every page.
    let wiki = f.wiki.clone().with_embedder(embedder());
    wiki.write_page(WritePageRequest {
        frontmatter: serde_json::json!({"kind": "procedure"}),
        tier: Tier::Procedural,
        ..req(
            ws,
            f.alpha,
            "procedures/quokka.md",
            "# Quokka care\n\nFeed the quokka at dawn. See [[notes/vault]].",
        )
    })
    .await
    .unwrap();
    wiki.write_page(req(
        ws,
        f.alpha,
        "notes/quokka-fact.md",
        "# Quokka fact\n\nA quokka is a small marsupial.",
    ))
    .await
    .unwrap();
    wiki.write_page(WritePageRequest {
        frontmatter: serde_json::json!({"kind": "fact", "entities": ["quokka"]}),
        ..req(
            ws,
            f.alpha,
            "notes/tagged.md",
            "# Tagged\n\nIsland wildlife.",
        )
    })
    .await
    .unwrap();
    // Links to the procedure without naming it in its text: the graph
    // stream's incoming side alone reaches it.
    f.store
        .writer
        .upsert_page(ai_memory_core::NewPage {
            workspace_id: ws,
            project_id: f.alpha,
            path: PagePath::new("notes/pointer.md").unwrap(),
            title: "Pointer".into(),
            body: "Island care handbook.".into(),
            tier: Tier::Semantic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: vec![ai_memory_core::LinkTarget {
                workspace: None,
                project: None,
                path: PagePath::new("procedures/quokka.md").unwrap(),
                relation: None,
            }],
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    let session_id = SessionId::new();
    f.store
        .writer
        .begin_session(NewSession {
            occurred_at: None,
            id: session_id,
            workspace_id: ws,
            project_id: f.alpha,
            agent_kind: AgentKind::Codex,
            cwd: None,
            actor_user: None,
            origin: None,
        })
        .await
        .unwrap();
    f.store
        .writer
        .insert_observation(ai_memory_core::Sanitized::new(
            NewObservation {
                occurred_at: None,
                session_id,
                workspace_id: ws,
                project_id: f.alpha,
                kind: ObservationKind::UserPrompt,
                extension: None,
                source_event: None,
                title: "prompt".into(),
                body: "where does the quokka sleep".into(),
                importance: 5,
                file_path: None,
            },
            &ai_memory_core::Sanitizer::builtin(),
        ))
        .await
        .unwrap();
    let session_path = format!("sessions/{session_id}.md");
    f.store
        .writer
        .upsert_page(ai_memory_core::NewPage {
            workspace_id: ws,
            project_id: f.alpha,
            path: PagePath::new(session_path.clone()).unwrap(),
            title: "A session".into(),
            body: "Reviewed the release.".into(),
            tier: Tier::Episodic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
    let app = web(
        &f,
        Recall {
            embedder: Some(embedder()),
            rerank: None,
        },
    );
    let base = "/web/search?q=quokka&workspace=default&project=alpha&limit=50";

    let (status, body) = get(&app, base, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let all = hit_paths(&body);
    for path in [
        "procedures/quokka.md",
        "notes/quokka-fact.md",
        "notes/tagged.md",
        "notes/vault.md",
        "notes/pointer.md",
        "notes/deploy.md",
        session_path.as_str(),
    ] {
        assert!(
            all.contains(&path.to_owned()),
            "control lacks {path}: {all:?}"
        );
    }

    for filter in [
        "kind=procedure",
        "tier=procedural",
        "kind=PROCEDURE&tier=procedural",
    ] {
        let (status, body) = get(&app, &format!("{base}&{filter}"), None).await;
        assert_eq!(status, StatusCode::OK, "{filter}: {body}");
        assert_eq!(
            hit_paths(&body),
            ["procedures/quokka.md"],
            "{filter}: {body}"
        );
    }
    // The other way round: the session page alone is episodic.
    let (_, body) = get(&app, &format!("{base}&tier=episodic"), None).await;
    assert_eq!(hit_paths(&body), [session_path], "{body}");
}

/// The cross-project search filters in SQL too: 60 better-ranked facts, more
/// than its candidate window for `limit=5`, cannot push a matching gotcha out.
#[tokio::test]
async fn cross_project_kind_filter_sees_past_the_candidate_window() {
    let f = fixture(false).await;
    let ws = f
        .store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    for n in 0..60 {
        f.store
            .writer
            .upsert_page(ai_memory_core::NewPage {
                workspace_id: ws,
                project_id: f.alpha,
                path: PagePath::new(format!("notes/crowd-{n}.md")).unwrap(),
                title: "Crowd".into(),
                body: "crowdword crowdword crowdword crowdword crowdword.".into(),
                tier: Tier::Semantic,
                frontmatter_json: serde_json::json!({}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
                expires_at: None,
                entities: Vec::new(),
                evidence: Vec::new(),
            })
            .await
            .unwrap();
    }
    f.wiki
        .write_page(WritePageRequest {
            frontmatter: serde_json::json!({"kind": "gotcha"}),
            ..req(
                ws,
                f.alpha,
                "gotchas/quiet.md",
                "# Quiet\n\nOne mention of crowdword among many other unrelated words \
                 about cats, rivers, mountains, bridges and old maps.",
            )
        })
        .await
        .unwrap();
    let app = web(&f, Recall::default());
    let base = "/web/search?q=crowdword&limit=5";

    let (_, body) = get(&app, base, None).await;
    assert!(
        !hit_paths(&body).contains(&"gotchas/quiet.md".to_owned()),
        "control: the gotcha must be ranked out of the first page: {body}"
    );
    let (_, body) = get(&app, &format!("{base}&kind=gotcha"), None).await;
    assert_eq!(hit_paths(&body), ["gotchas/quiet.md"], "{body}");
}
