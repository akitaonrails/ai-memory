//! 2.2.0 retrieval features, proven end to end through the real MCP tools.
//!
//! Each of these signals is already covered at the `Store` / `ReaderPool`
//! layer, but that coverage stops one layer below the wire: it never proves
//! that `memory_query` / `memory_briefing` actually thread the store result
//! into the user-visible JSON a client receives. These tests seed the store
//! through the write path, then drive the production tools over the JSON-RPC
//! HTTP transport (the same shape a real MCP client sends) and assert on the
//! decoded tool payload:
//!
//! 1. `memory_query(as_of=T)` — time-travel returns the version valid then,
//!    while the plain query returns only the latest (#656).
//! 2. `memory_query(explain=true)` `graph_via.edge` — a typed relation edge
//!    names why a neighbour surfaced (P3, 2.2.0).
//! 3. `memory_query(explain=true)` `evidence_count` — belief-strength
//!    `page_evidence` reaches explain output (P2 / V63).
//! 4. `memory_briefing(settled_first=true)` — standing rule/decision pages,
//!    ordered by evidence then recency (P4).
//!
//! The `[retrieval] query_intent` / `abstract_vectors` opt-in ranking signals
//! are deliberately NOT retested here: they are driven by process-level
//! `Config`/`RetrievalTuning` that the tool layer reads once at startup and
//! cannot be flipped per-call without an `unsafe` env mutation. Their store
//! coverage (`ai-memory-store/tests/suite/retrieval_tuning_streams.rs`)
//! already exercises the ordering; see the report note.

use ai_memory_core::{
    LinkTarget, NewPage, PageEvidence, PageEvidenceKind, PagePath, ProjectId, Relation, Tier,
    WorkspaceId,
};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::Store;
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const WS: &str = "default";
const PROJ: &str = "scratch";

struct Harness {
    /// The server mounted with no auth layer — a local stdio / in-process
    /// client, which is all these read-only retrieval tools need.
    router: Router,
    store: Store,
    ws: WorkspaceId,
    proj: ProjectId,
    _tmp: TempDir,
}

fn mount(server: AiMemoryServer) -> Router {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true),
    );
    Router::new().nest_service("/mcp", service)
}

async fn harness() -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, PROJ, None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    let server =
        AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj).with_wiki(wiki);

    Harness {
        router: mount(server),
        store,
        ws,
        proj,
        _tmp: tmp,
    }
}

/// Drive one tool over the real JSON-RPC transport and return the decoded
/// tool payload (the JSON a client actually renders).
async fn call(router: &Router, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_string()))
        .expect("mcp req");
    let resp = router.clone().oneshot(req).await.expect("oneshot");
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .expect("body");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}: {e}"));
    if let Some(err) = v.get("error") {
        panic!("JSON-RPC error: {err}\nfull: {text}");
    }
    let joined = v
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("missing result.content: {text}"))
        .iter()
        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&joined).unwrap_or_else(|e| panic!("tool text not JSON: {joined}: {e}"))
}

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, title: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: title.into(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned: false,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

/// The `hits` array of a `memory_query` response as query-side JSON.
fn hits(resp: &Value) -> &Vec<Value> {
    resp.get("hits")
        .and_then(|h| h.as_array())
        .unwrap_or_else(|| panic!("no hits array: {resp}"))
}

fn hit_for<'a>(resp: &'a Value, path: &str) -> Option<&'a Value> {
    hits(resp)
        .iter()
        .find(|h| h.get("path").and_then(|p| p.as_str()) == Some(path))
}

fn query_args(query: &str, extra: Value) -> Value {
    let mut base = json!({
        "query": query,
        "workspace": WS,
        "project": PROJ,
        "limit": 10,
    });
    let obj = base.as_object_mut().unwrap();
    for (k, v) in extra.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    base
}

/// 1. `as_of` is a time-travel lookup: the version that was valid at the
/// instant answers, and the plain query returns only the latest. Mirrors
/// the store `_at` semantics but proves it survives the tool wiring.
#[tokio::test]
async fn memory_query_as_of_returns_the_superseded_version_through_the_tool() {
    let h = harness().await;

    // v1: an entity-carrying page whose body mentions postgres.
    let mut p = page(h.ws, h.proj, "notes/db.md", "DB", "we use postgres");
    p.entities = vec!["postgres".into()];
    h.store.writer.upsert_page(p.clone()).await.unwrap();

    // An instant strictly between the two writes.
    let between = jiff::Timestamp::now().to_string();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;

    // v2 supersedes: postgres is gone from both body and entities.
    p.body = "we migrated to sqlite".into();
    p.entities = vec!["sqlite".into()];
    h.store.writer.upsert_page(p).await.unwrap();

    // Time-travel to `between`: the superseded version answers, and its
    // OLD body content ("postgres") is what the snippet is drawn from.
    let historical = call(
        &h.router,
        "memory_query",
        query_args("postgres", json!({ "as_of": between, "explain": true })),
    )
    .await;
    let hit = hit_for(&historical, "notes/db.md")
        .expect("the version valid at `between` must answer the as_of query");
    let snippet = hit.get("snippet").and_then(|s| s.as_str()).unwrap_or("");
    assert!(
        snippet.to_lowercase().contains("postgres"),
        "the as_of snippet must be drawn from the OLD version body: {snippet:?}"
    );
    // as_of runs the entity + version-filtered FTS streams (docs/temporal.md).
    let streams = historical
        .get("streams_active")
        .and_then(|s| s.as_array())
        .expect("explain must report streams_active");
    let stream_names: Vec<&str> = streams.iter().filter_map(|s| s.as_str()).collect();
    assert!(
        stream_names.contains(&"entity") && stream_names.contains(&"fts"),
        "as_of should run entity + fts, got {stream_names:?}"
    );

    // The plain query (no as_of) sees only the latest version, whose body
    // no longer mentions postgres — so the page must NOT surface.
    let now = call(&h.router, "memory_query", query_args("postgres", json!({}))).await;
    assert!(
        hit_for(&now, "notes/db.md").is_none(),
        "a plain query must not return a page whose latest version dropped the term: {now}"
    );

    // ...and querying the NEW term without as_of does find it, confirming
    // the page still exists and only the version resolution differs.
    let migrated = call(&h.router, "memory_query", query_args("migrated", json!({}))).await;
    assert!(
        hit_for(&migrated, "notes/db.md").is_some(),
        "the latest version's own body must be findable: {migrated}"
    );
}

/// 2. A typed relation edge names *why* a neighbour surfaced. Seed a page
/// linked to another by a `fixes` edge; `explain=true` must carry
/// `graph_via.edge = "fixes"` on the neighbour hit.
#[tokio::test]
async fn memory_query_explain_surfaces_the_typed_graph_edge() {
    let h = harness().await;

    // The neighbour: no query term of its own, reachable only via the edge.
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            h.proj,
            "target.md",
            "Target",
            "neighbor-only content",
        ))
        .await
        .unwrap();

    // The seed: matches the FTS query, and `fixes` the neighbour.
    let mut source = page(h.ws, h.proj, "source.md", "Source", "needle source content");
    source.links = vec![LinkTarget {
        workspace: None,
        project: None,
        path: PagePath::new("target.md").unwrap(),
        relation: Some(Relation::Fixes),
    }];
    h.store.writer.upsert_page(source).await.unwrap();

    let resp = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({ "explain": true })),
    )
    .await;

    let target = hit_for(&resp, "target.md")
        .expect("the fixes-linked neighbour must surface via the graph stream");
    let via = target
        .pointer("/score_details/graph_via")
        .unwrap_or_else(|| panic!("no graph_via on the neighbour hit: {target}"));
    assert_eq!(
        via.get("seed_path").and_then(|s| s.as_str()),
        Some("source.md"),
        "graph_via must name the seed the edge was followed from: {via}"
    );
    assert_eq!(
        via.get("edge").and_then(|e| e.as_str()),
        Some("fixes"),
        "the typed edge kind must reach the tool's explain output: {via}"
    );
}

/// 3. `page_evidence` (belief strength) reaches explain output. Cite a page
/// from two distinct sessions; `explain=true` must report `evidence_count`
/// 2 for it and 0 for a page with no evidence.
#[tokio::test]
async fn memory_query_explain_reports_evidence_count() {
    let h = harness().await;

    let page_a = page(h.ws, h.proj, "a.md", "A", "evidence probe alpha content");
    let page_b = page(h.ws, h.proj, "b.md", "B", "evidence probe beta content");
    h.store.writer.upsert_page(page_a.clone()).await.unwrap();
    h.store.writer.upsert_page(page_b).await.unwrap();

    // Cite a.md from two distinct sessions. The body is byte-identical each
    // time, so this hits the content short-circuit (no new version) and only
    // the page_evidence rows accrue — exactly the store-test shape.
    let mut with_evidence = page_a.clone();
    with_evidence.evidence = vec![PageEvidence {
        kind: PageEvidenceKind::Session,
        source_id: "session-1".into(),
    }];
    h.store
        .writer
        .upsert_page(with_evidence.clone())
        .await
        .unwrap();
    with_evidence.evidence = vec![PageEvidence {
        kind: PageEvidenceKind::Session,
        source_id: "session-2".into(),
    }];
    h.store.writer.upsert_page(with_evidence).await.unwrap();

    let resp = call(
        &h.router,
        "memory_query",
        query_args("evidence probe", json!({ "explain": true })),
    )
    .await;

    let a = hit_for(&resp, "a.md").expect("a.md must surface");
    let b = hit_for(&resp, "b.md").expect("b.md must surface");
    assert_eq!(
        a.pointer("/score_details/evidence_count")
            .and_then(Value::as_u64),
        Some(2),
        "two distinct sessions cited a.md: {a}"
    );
    assert_eq!(
        b.pointer("/score_details/evidence_count")
            .and_then(Value::as_u64),
        Some(0),
        "b.md has no evidence rows — explain still reports 0: {b}"
    );
}

/// 4. `settled_first` leads the briefing with standing rule/decision pages,
/// ordered by evidence count then recency, excluding every other kind.
#[tokio::test]
async fn memory_briefing_settled_first_orders_by_evidence_then_recency() {
    let h = harness().await;

    // decision-a: two evidence rows -> highest standing.
    let mut decision_a = page(
        h.ws,
        h.proj,
        "decisions/decision-a.md",
        "Decision A",
        "body a",
    );
    decision_a.evidence = vec![
        PageEvidence {
            kind: PageEvidenceKind::Session,
            source_id: "session-1".into(),
        },
        PageEvidence {
            kind: PageEvidenceKind::Session,
            source_id: "session-2".into(),
        },
    ];
    h.store.writer.upsert_page(decision_a).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    // rule-a: zero evidence, written before decision-b.
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            h.proj,
            "_rules/rule-a.md",
            "Rule A",
            "body rule",
        ))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    // decision-b: zero evidence, written last -> beats rule-a on recency.
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            h.proj,
            "decisions/decision-b.md",
            "Decision B",
            "body b",
        ))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    // A plain note must never appear in `settled`.
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            h.proj,
            "notes/plain-note.md",
            "Note",
            "body note",
        ))
        .await
        .unwrap();

    // Default briefing: `settled` is absent/empty.
    let default = call(
        &h.router,
        "memory_briefing",
        json!({ "workspace": WS, "project": PROJ }),
    )
    .await;
    assert!(
        default
            .get("settled")
            .and_then(|s| s.as_array())
            .is_none_or(|a| a.is_empty()),
        "default briefing must not carry settled: {default}"
    );

    // Opt in: settled leads with rule/decision pages in evidence-then-recency
    // order, plain note excluded.
    let settled_resp = call(
        &h.router,
        "memory_briefing",
        json!({ "workspace": WS, "project": PROJ, "settled_first": true }),
    )
    .await;
    let settled = settled_resp
        .get("settled")
        .and_then(|s| s.as_array())
        .expect("settled_first must populate settled");
    let paths: Vec<&str> = settled
        .iter()
        .filter_map(|p| p.get("path").and_then(|x| x.as_str()))
        .collect();
    assert_eq!(
        paths,
        vec![
            "decisions/decision-a.md",
            "decisions/decision-b.md",
            "_rules/rule-a.md",
        ],
        "expected evidence-count-then-recency order: {paths:?}"
    );
    assert!(
        !paths.contains(&"notes/plain-note.md"),
        "a plain note must not appear in settled"
    );
    assert_eq!(
        settled[0].get("evidence_count").and_then(Value::as_u64),
        Some(2)
    );
    assert_eq!(
        settled[1].get("evidence_count").and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        settled[0].get("kind").and_then(|k| k.as_str()),
        Some("decision")
    );
    assert_eq!(
        settled[2].get("kind").and_then(|k| k.as_str()),
        Some("rule")
    );
}
