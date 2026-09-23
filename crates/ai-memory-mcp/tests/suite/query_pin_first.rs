//! 2.4 "pin before search": pinned pages become standing context that is
//! surfaced *ahead of* the fused search hits (opt-in `memory_query`
//! `pin_first=true`) and carried on the `memory_briefing` snapshot's `pinned`
//! list, proven end to end through the real MCP tools.
//!
//! Pinned pages otherwise only earn a small post-RRF authority bump; they are
//! never placed before the search result, and the briefing never listed them by
//! the `pinned` column. These tests seed the store through the write path, then
//! drive the production tools over the JSON-RPC HTTP transport and assert on the
//! decoded tool payload:
//!
//! 1. `memory_query(pin_first=false)` — default: a pinned page is NOT forced to
//!    the front (byte-identical ordering to today).
//! 2. `memory_query(pin_first=true)` — bounded pinned latest pages lead the
//!    result, deduped against the fused hits (a pinned page that also matches
//!    the query appears exactly once), each marked `pinned: true`.
//! 3. `memory_briefing` — the `pinned` list carries only pinned latest pages
//!    when pins exist, and is absent/empty when none do.

use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
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

fn page(ws: WorkspaceId, proj: ProjectId, path: &str, body: &str, pinned: bool) -> NewPage {
    NewPage {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        title: path.to_string(),
        body: body.into(),
        tier: Tier::Semantic,
        frontmatter_json: serde_json::json!({}),
        pinned,
        links: Vec::new(),
        author_id: None,
        expires_at: None,
        entities: Vec::new(),
        evidence: Vec::new(),
    }
}

fn hits(resp: &Value) -> &Vec<Value> {
    resp.get("hits")
        .and_then(|h| h.as_array())
        .unwrap_or_else(|| panic!("no hits array: {resp}"))
}

fn hit_paths(resp: &Value) -> Vec<&str> {
    hits(resp)
        .iter()
        .filter_map(|h| h.get("path").and_then(|p| p.as_str()))
        .collect()
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

/// 1 + 2. A pinned page that does NOT match the query only leads the result
/// when `pin_first=true`; by default the search ordering is untouched.
#[tokio::test]
async fn memory_query_pin_first_prepends_pinned_context() {
    let h = harness().await;

    // A pinned "standing context" page with no query term of its own.
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            h.proj,
            "_slots/current-focus.md",
            "the current sprint focus is unrelated standing context",
            true,
        ))
        .await
        .unwrap();

    // An unpinned page that actually matches the query.
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            h.proj,
            "notes/topic.md",
            "the widget subsystem needle lives here",
            false,
        ))
        .await
        .unwrap();

    // Default (pin_first absent / false): the pinned page must NOT be forced
    // ahead of the matching hit — it doesn't match "needle" at all.
    let default = call(&h.router, "memory_query", query_args("needle", json!({}))).await;
    assert!(
        !hit_paths(&default).contains(&"_slots/current-focus.md"),
        "default query must not surface a pinned page that doesn't match: {default}"
    );
    assert!(
        hit_paths(&default).contains(&"notes/topic.md"),
        "the matching page must surface: {default}"
    );

    // pin_first=true: the pinned page leads, then the fused hit follows.
    let pinned_first = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({ "pin_first": true })),
    )
    .await;
    let paths = hit_paths(&pinned_first);
    assert_eq!(
        paths.first(),
        Some(&"_slots/current-focus.md"),
        "pin_first must place standing pinned context first: {paths:?}"
    );
    assert!(
        paths.contains(&"notes/topic.md"),
        "the fused search hit must still be present: {paths:?}"
    );
    // The prepended pin is marked so a client can tell it apart.
    let lead = &hits(&pinned_first)[0];
    assert_eq!(
        lead.get("pinned").and_then(Value::as_bool),
        Some(true),
        "the prepended pin must be marked pinned:true: {lead}"
    );
}

/// 2 (dedup + bound). A pinned page that ALSO matches the query appears exactly
/// once, at the front; the pinned context is bounded.
#[tokio::test]
async fn memory_query_pin_first_dedups_and_bounds() {
    let h = harness().await;

    // A pinned page that DOES match the query.
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            h.proj,
            "_slots/pinned-match.md",
            "this pinned page mentions needle directly",
            true,
        ))
        .await
        .unwrap();
    // A second matching unpinned page so there is a fused hit too.
    h.store
        .writer
        .upsert_page(page(
            h.ws,
            h.proj,
            "notes/other.md",
            "another needle match here",
            false,
        ))
        .await
        .unwrap();

    let resp = call(
        &h.router,
        "memory_query",
        query_args("needle", json!({ "pin_first": true })),
    )
    .await;
    let paths = hit_paths(&resp);
    assert_eq!(
        paths.first(),
        Some(&"_slots/pinned-match.md"),
        "the pinned match must lead: {paths:?}"
    );
    let occurrences = paths
        .iter()
        .filter(|p| **p == "_slots/pinned-match.md")
        .count();
    assert_eq!(
        occurrences, 1,
        "a pinned page that also matches must appear exactly once (deduped): {paths:?}"
    );
    // Bounded: never exceeds the requested limit.
    assert!(
        hits(&resp).len() <= 10,
        "pin_first result must stay within the requested limit: {}",
        hits(&resp).len()
    );
}

/// 3. The briefing carries a `pinned` list of pinned latest pages when they
/// exist, and omits/empties it when none do.
#[tokio::test]
async fn memory_briefing_carries_pinned_list() {
    let h = harness().await;

    // No pins yet: the briefing must not carry a non-empty pinned list.
    let empty = call(
        &h.router,
        "memory_briefing",
        json!({ "workspace": WS, "project": PROJ }),
    )
    .await;
    assert!(
        empty
            .get("pinned")
            .and_then(|p| p.as_array())
            .is_none_or(|a| a.is_empty()),
        "briefing with no pins must not carry a pinned list: {empty}"
    );

    // A pinned page + an unpinned page.
    h.store
        .writer
        .upsert_page(page(h.ws, h.proj, "_slots/standing.md", "standing", true))
        .await
        .unwrap();
    h.store
        .writer
        .upsert_page(page(h.ws, h.proj, "notes/plain.md", "plain", false))
        .await
        .unwrap();

    let resp = call(
        &h.router,
        "memory_briefing",
        json!({ "workspace": WS, "project": PROJ }),
    )
    .await;
    let pinned = resp
        .get("pinned")
        .and_then(|p| p.as_array())
        .expect("briefing must carry a pinned list once pins exist");
    let paths: Vec<&str> = pinned
        .iter()
        .filter_map(|p| p.get("path").and_then(|x| x.as_str()))
        .collect();
    assert_eq!(
        paths,
        vec!["_slots/standing.md"],
        "only pinned latest pages appear in briefing.pinned: {paths:?}"
    );
}
