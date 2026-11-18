//! #2038 `min_relevance` relevance floor, proven end to end through the real
//! `memory_query` tool. A position-based stub reranker assigns a known
//! calibrated score to each candidate, so the number of hits that survive a
//! floor is deterministic regardless of which seeded page lands where in the
//! fused order. Covers: the floor excludes below-threshold hits before the
//! count limit; precedence (call → project → global); a transient rerank
//! failure skips the filter and reports `relevance_filter_unavailable` rather
//! than dropping every hit; and the adversarial guards — no reranker rejects a
//! floor, an out-of-range call value is rejected, and `global`/`as_of` reject it.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::{RetrievalCriteria, Store};
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::Request;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::ServiceExt;

const WS: &str = "default";
const PROJ: &str = "scratch";
/// Fewer than `limit * RERANK_OVERFETCH` and under the candidate cap, so every
/// seeded page is a scored rerank candidate — no unscored fused tail muddies
/// the counts.
const SEEDED_PAGES: usize = 20;
/// The first `HIGH_COUNT` candidates (in fused order) get `HIGH`, the rest
/// `LOW`. A floor between `LOW` and `HIGH` therefore keeps exactly `HIGH_COUNT`.
const HIGH_COUNT: usize = 4;
const HIGH: f32 = 0.9;
const LOW: f32 = 0.1;

/// What the stub reranker does with a query's candidates.
#[derive(Clone, Copy)]
enum RerankMode {
    /// Score the first `HIGH_COUNT` candidates `HIGH`, the rest `LOW`.
    Ramp,
    /// Fail with a provider error, so the stage keeps the fused order unscored
    /// (the transient-failure path).
    Fail,
}

struct StubReranker {
    mode: RerankMode,
}

#[async_trait::async_trait]
impl ai_memory_llm::Reranker for StubReranker {
    fn name(&self) -> &'static str {
        "stub"
    }
    fn model(&self) -> &str {
        "stub-model"
    }
    async fn rerank(
        &self,
        _query: &str,
        candidates: &[ai_memory_llm::RerankCandidate],
    ) -> ai_memory_llm::LlmResult<Vec<ai_memory_llm::RerankScore>> {
        match self.mode {
            RerankMode::Ramp => Ok(candidates
                .iter()
                .enumerate()
                .map(|(idx, candidate)| ai_memory_llm::RerankScore {
                    id: candidate.id.clone(),
                    relevance: if idx < HIGH_COUNT { HIGH } else { LOW },
                })
                .collect()),
            RerankMode::Fail => Err(ai_memory_llm::LlmError::UnexpectedShape(
                "stub transient failure".into(),
            )),
        }
    }
}

struct Harness {
    router: Router,
    store: Store,
    proj: ProjectId,
    _tmp: TempDir,
}

fn mount(server: AiMemoryServer) -> Router {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    Router::new().nest_service("/mcp", service)
}

/// Build a harness. `reranker` attaches a stub reranker (`None` leaves the
/// server RRF-only); `global_floor` sets the server-wide `[retrieval]
/// min_relevance`.
async fn harness(reranker: Option<RerankMode>, global_floor: Option<f32>) -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store.writer.get_or_create_workspace(WS).await.expect("ws");
    let proj = store
        .writer
        .get_or_create_project(ws, PROJ, None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    for i in 0..SEEDED_PAGES {
        store
            .writer
            .upsert_page(page(
                ws,
                proj,
                &format!("notes/alpha-{i:02}.md"),
                "alpha shared retrieval term for the min relevance test",
            ))
            .await
            .expect("seed page");
    }
    let mut server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki)
        .with_default_min_relevance(global_floor);
    if let Some(mode) = reranker {
        server = server.with_reranker(Arc::new(StubReranker { mode }));
    }
    Harness {
        router: mount(server),
        store,
        proj,
        _tmp: tmp,
    }
}

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.to_string(),
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

/// Raw JSON-RPC envelope (either `result` or `error`), without panicking on an
/// error — the adversarial tests assert on the error.
async fn call_raw(router: &Router, name: &str, arguments: Value) -> Value {
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
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}: {e}"))
}

/// Decode a successful `memory_query` tool response body.
async fn query(h: &Harness, arguments: Value) -> Value {
    let v = call_raw(&h.router, "memory_query", arguments).await;
    if let Some(err) = v.get("error") {
        panic!("unexpected JSON-RPC error: {err}\nfull: {v}");
    }
    let joined = v
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("missing result.content: {v}"))
        .iter()
        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    serde_json::from_str(&joined).unwrap_or_else(|e| panic!("tool text not JSON: {joined}: {e}"))
}

fn hit_count(resp: &Value) -> usize {
    resp.get("hits")
        .and_then(|h| h.as_array())
        .unwrap_or_else(|| panic!("no hits array: {resp}"))
        .len()
}

/// Expect a JSON-RPC error whose message contains `needle`.
async fn expect_error(h: &Harness, arguments: Value, needle: &str) {
    let v = call_raw(&h.router, "memory_query", arguments).await;
    let message = v
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("expected a JSON-RPC error, got: {v}"));
    assert!(
        message.contains(needle),
        "error {message:?} should mention {needle:?}"
    );
}

async fn set_project_floor(h: &Harness, floor: Option<f32>) {
    h.store
        .writer
        .set_project_retrieval_criteria(
            h.proj,
            RetrievalCriteria {
                min_relevance: floor,
                ..RetrievalCriteria::default()
            },
        )
        .await
        .unwrap();
}

/// With a reranker and an explicit call floor between `LOW` and `HIGH`, only the
/// `HIGH_COUNT` above-floor hits come back — fewer than the default limit, so
/// the floor is doing the excluding, not the count.
#[tokio::test]
async fn call_floor_excludes_below_threshold_hits() {
    let h = harness(Some(RerankMode::Ramp), None).await;
    // No floor: the default limit (10) hits come back.
    let baseline = query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ }),
    )
    .await;
    assert_eq!(hit_count(&baseline), 10);
    assert!(baseline.get("relevance_filter_unavailable").is_none());

    // Floor 0.5 keeps only the HIGH_COUNT scored at 0.9.
    let filtered = query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ, "min_relevance": 0.5 }),
    )
    .await;
    assert_eq!(hit_count(&filtered), HIGH_COUNT);
    assert!(filtered.get("relevance_filter_unavailable").is_none());

    // A floor above every score empties the result honestly — no error.
    let empty = query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ, "min_relevance": 0.95 }),
    )
    .await;
    assert_eq!(hit_count(&empty), 0);
}

/// Precedence: an explicit call floor overrides both the project's stored floor
/// and the global default, and the project's stored floor overrides the global.
#[tokio::test]
async fn floor_precedence_is_call_then_project_then_global() {
    // Global floor 0.9 keeps only the HIGH_COUNT; a looser stored project floor
    // (0.0) overrides it to keep the full limit.
    let h = harness(Some(RerankMode::Ramp), Some(0.9)).await;
    let global_only = query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ }),
    )
    .await;
    assert_eq!(hit_count(&global_only), HIGH_COUNT, "global floor applies");

    set_project_floor(&h, Some(0.0)).await;
    let project_wins = query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ }),
    )
    .await;
    assert_eq!(
        hit_count(&project_wins),
        10,
        "a looser project floor overrides the stricter global one"
    );

    // An explicit call floor overrides the loose project floor, back to strict.
    let call_wins = query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ, "min_relevance": 0.5 }),
    )
    .await;
    assert_eq!(
        hit_count(&call_wins),
        HIGH_COUNT,
        "an explicit call floor overrides the project floor"
    );
}

/// A transient rerank failure leaves every candidate unscored, so the floor
/// cannot be applied. The fused order is returned unfiltered and the response
/// says so — never silently emptied.
#[tokio::test]
async fn transient_rerank_failure_skips_the_filter_and_reports() {
    let h = harness(Some(RerankMode::Fail), None).await;
    let resp = query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ, "min_relevance": 0.5 }),
    )
    .await;
    // Fused order, truncated to the default limit — not emptied by the floor.
    assert_eq!(hit_count(&resp), 10);
    let note = resp
        .get("relevance_filter_unavailable")
        .and_then(Value::as_str)
        .expect("relevance_filter_unavailable should be present on a transient failure");
    assert!(note.contains("min_relevance"), "note: {note}");
}

/// Adversarial: a floor needs the reranker's calibrated score. With no reranker
/// configured, a resolved floor (here from the call) is rejected rather than
/// applied to the uncalibrated fused rank. A control query with no floor works.
#[tokio::test]
async fn no_reranker_rejects_a_floor_but_allows_a_plain_query() {
    let h = harness(None, None).await;
    // Control: a plain query still works.
    let ok = query(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ }),
    )
    .await;
    assert_eq!(hit_count(&ok), 10);

    expect_error(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ, "min_relevance": 0.5 }),
        "reranker",
    )
    .await;

    // A project-stored floor with no reranker is rejected the same way — not
    // silently ignored, since that would return more than the floor allows.
    set_project_floor(&h, Some(0.5)).await;
    expect_error(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ }),
        "reranker",
    )
    .await;
}

/// Adversarial: an out-of-range or non-finite call value is rejected, and the
/// floor is refused on the `global` and `as_of` paths that cannot rank.
#[tokio::test]
async fn floor_is_rejected_when_it_cannot_be_honored() {
    let h = harness(Some(RerankMode::Ramp), None).await;

    expect_error(
        &h,
        json!({ "query": "alpha", "workspace": WS, "project": PROJ, "min_relevance": 1.5 }),
        "between 0.0 and 1.0",
    )
    .await;

    expect_error(
        &h,
        json!({ "query": "alpha", "global": true, "min_relevance": 0.5 }),
        "not supported for global",
    )
    .await;

    expect_error(
        &h,
        json!({
            "query": "alpha",
            "workspace": WS,
            "project": PROJ,
            "as_of": "2026-01-01T00:00:00Z",
            "min_relevance": 0.5,
        }),
        "not supported for as_of",
    )
    .await;
}
