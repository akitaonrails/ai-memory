//! Which slot namespace an operator owns, when the ingress names them by OIDC
//! subject alone.
//!
//! A slot is not an ordinary page. `_slots/<key>/…` bodies are injected
//! verbatim into that operator's next session brief, so the two decisions here
//! are one decision seen from two sides:
//!
//! * the **write** door namespaces a hand-written slot page into
//!   `_slots/<key>/…` ([`ai_memory_core::slot_placement`]);
//! * the **read** filter admits `_slots/<viewer>/*`
//!   ([`ai_memory_core::SlotVisibility`]).
//!
//! Key them on different fields and the page is force-pinned, write-only and
//! permanently invisible to its own owner — or, worse, re-homing never happens
//! and a "personal" slot lands on the SHARED path that reaches every other
//! operator's brief. Both halves therefore go through
//! [`ai_memory_core::ActorContext::identity_key`], and every test below asserts
//! them together rather than one at a time.
//!
//! Driven through the production JSON-RPC transport with the production
//! `require_bearer` middleware in front, so the rung under test is the one an
//! operator actually configures: `[auth].actor_proxy_secret` plus an ingress
//! asserting `X-Memory-Actor-Sub` with no `X-Memory-Actor-User`.

use ai_memory_core::ActorContext;
use ai_memory_mcp::AiMemoryServer;
use ai_memory_mcp::auth::{AuthState, require_bearer};
use ai_memory_store::Store;
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

const ROOT_TOKEN: &str = "the-root-token";
const PROXY_SECRET: &str = "proxy-shared-secret";
const ALICE_SUB: &str = "oidc-subject-alice";
const BOB_SUB: &str = "oidc-subject-bob";

struct Harness {
    http: Router,
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

/// `per_user_slots` is `[slots] per_user`. The trusted proxy is always
/// configured — it is what lets the ingress assert an identity at all — so
/// `per_user_slots: false` is the DEFAULT-CONFIG case with the identity still
/// present, which is the strictly harder thing to keep unchanged.
async fn harness(per_user_slots: bool) -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .expect("proj");
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .expect("wiki")
        .with_per_user_slots(per_user_slots);

    let server = AiMemoryServer::new(
        store.reader.clone(),
        store.writer.clone(),
        ws,
        store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .expect("proj"),
    )
    .with_wiki(wiki)
    .with_per_user_slots(per_user_slots)
    // Also what makes the deployment distinguish operators without a `users`
    // row, so the admin hatch in `place_slot_write` stays shut.
    .with_trusted_proxy_identity(true);

    let auth_state = AuthState::new(Some(ROOT_TOKEN.to_string()))
        .with_root_actor(ActorContext {
            user: Some("dj".to_string()),
            ..ActorContext::default()
        })
        .with_trusted_proxy(PROXY_SECRET);

    let http = mount(server).layer(axum::middleware::from_fn_with_state(
        Arc::new(auth_state),
        require_bearer,
    ));

    Harness { http, _tmp: tmp }
}

/// The raw JSON-RPC result, so a test can assert on a REFUSAL as well as a
/// success. `Err` carries the JSON-RPC error message.
async fn try_call(
    router: &Router,
    name: &str,
    arguments: Value,
    headers: &[(&str, &str)],
) -> Result<Value, String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let mut req = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = router
        .clone()
        .oneshot(req.body(Body::from(body.to_string())).expect("mcp req"))
        .await
        .expect("oneshot");
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .expect("body");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    let v: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("non-JSON: {text}: {e}"));
    if let Some(err) = v.get("error") {
        return Err(err.to_string());
    }
    let joined = v
        .pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("missing result.content: {text}"))
        .iter()
        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(serde_json::from_str(&joined)
        .unwrap_or_else(|e| panic!("tool text not JSON: {joined}: {e}")))
}

fn proxied(header: &'static str, value: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![
        ("authorization", "Bearer the-root-token"),
        ("x-memory-actor-proxy-secret", PROXY_SECRET),
        (header, value),
    ]
}

/// Where a slot write actually LANDED, per the tool's own response.
async fn write_slot(
    router: &Router,
    path: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> Result<String, String> {
    let written = try_call(
        router,
        "memory_write_page",
        json!({
            "workspace": "default",
            "project": "scratch",
            "path": path,
            "body": body,
        }),
        headers,
    )
    .await?;
    Ok(written
        .get("path")
        .and_then(|p| p.as_str())
        .unwrap_or_else(|| panic!("response carries no written path: {written}"))
        .to_string())
}

/// The slot paths this caller's own briefing lists — the read half.
async fn brief_slots(router: &Router, headers: &[(&str, &str)]) -> Vec<String> {
    let snapshot = try_call(
        router,
        "memory_briefing",
        json!({ "workspace": "default", "project": "scratch" }),
        headers,
    )
    .await
    .expect("briefing is read-only and must never be refused");
    snapshot
        .get("slots")
        .and_then(|s| s.as_array())
        .unwrap_or_else(|| panic!("briefing carries no slots: {snapshot}"))
        .iter()
        .filter_map(|s| s.get("path").and_then(|p| p.as_str()))
        .map(str::to_owned)
        .collect()
}

/// THE PAIR, in one test. A sub-only operator's personal slot must land in
/// their namespace AND come back in their own brief; asserting either half
/// alone is what let the two drift apart.
#[tokio::test]
async fn sub_only_operators_slot_write_lands_where_their_own_brief_reads() {
    let h = harness(true).await;
    let alice = proxied("x-memory-actor-sub", ALICE_SUB);

    let landed = write_slot(&h.http, "_slots/current-focus.md", "alice only", &alice)
        .await
        .expect("a shared-slot write is re-homed, not refused");
    assert_eq!(
        landed,
        format!("_slots/{ALICE_SUB}/current-focus.md"),
        "a sub-only operator's personal slot landed on the project-wide path, \
         whose body reaches EVERY operator's session brief",
    );

    let seen = brief_slots(&h.http, &alice).await;
    assert!(
        seen.contains(&landed),
        "the page landed at {landed} but its own owner's brief lists {seen:?}",
    );
}

/// The read half of the same rule from the other side: one sub-only operator
/// must not be handed another's personal slot.
#[tokio::test]
async fn sub_only_operator_does_not_see_another_operators_personal_slot() {
    let h = harness(true).await;
    let alice = proxied("x-memory-actor-sub", ALICE_SUB);
    let bob = proxied("x-memory-actor-sub", BOB_SUB);

    let landed = write_slot(&h.http, "_slots/current-focus.md", "alice only", &alice)
        .await
        .expect("write");

    let bobs = brief_slots(&h.http, &bob).await;
    assert!(
        !bobs.contains(&landed),
        "Bob was handed Alice's personal slot: {bobs:?}",
    );
    assert!(
        !bobs.iter().any(|p| p.contains(ALICE_SUB)),
        "Bob's brief names Alice's namespace: {bobs:?}",
    );
}

/// A sub-only operator owns their namespace outright: naming it explicitly is
/// allowed, and another operator's is still refused.
#[tokio::test]
async fn sub_only_operator_owns_their_namespace_and_no_other() {
    let h = harness(true).await;
    let alice = proxied("x-memory-actor-sub", ALICE_SUB);

    let own = format!("_slots/{ALICE_SUB}/current-focus.md");
    assert_eq!(
        write_slot(&h.http, &own, "alice only", &alice)
            .await
            .expect("an operator was refused their OWN slot namespace"),
        own,
    );

    let err = write_slot(
        &h.http,
        &format!("_slots/{BOB_SUB}/current-focus.md"),
        "planted",
        &alice,
    )
    .await
    .expect_err("Alice must not write into Bob's slot namespace");
    assert!(err.contains("another operator"), "{err}");
}

/// An OIDC subject shaped like a URL cannot be a path segment, so it cannot own
/// a namespace either — and must NOT fall back to the shared slot every other
/// operator reads at session start. The existing `Unnamespaceable` refusal, kept
/// intact rather than weakened to accommodate subjects.
#[tokio::test]
async fn a_subject_that_cannot_be_a_path_segment_is_refused_not_shared() {
    let h = harness(true).await;
    let url_sub = proxied("x-memory-actor-sub", "https://issuer.example/users/7");

    let err = write_slot(&h.http, "_slots/current-focus.md", "planted", &url_sub)
        .await
        .expect_err("an unnamespaceable subject must not be handed the shared slot");
    assert!(err.contains("namespace"), "{err}");
    assert!(
        brief_slots(&h.http, &url_sub).await.is_empty(),
        "nothing may have been written",
    );
}

/// NON-REGRESSION. An operator the ingress names with a username behaves
/// exactly as before: `identity_key` returns `user` whenever it is present, so
/// every assertion here is the pre-change behaviour verbatim.
#[tokio::test]
async fn named_operator_slots_are_unchanged() {
    let h = harness(true).await;
    let alice = proxied("x-memory-actor-user", "alice");
    let bob = proxied("x-memory-actor-user", "bob");

    assert_eq!(
        write_slot(&h.http, "_slots/current-focus.md", "alice only", &alice)
            .await
            .expect("write"),
        "_slots/alice/current-focus.md",
    );
    assert!(
        brief_slots(&h.http, &alice)
            .await
            .contains(&"_slots/alice/current-focus.md".to_string()),
    );
    assert!(
        !brief_slots(&h.http, &bob)
            .await
            .contains(&"_slots/alice/current-focus.md".to_string()),
    );
    assert!(
        write_slot(&h.http, "_slots/alice/x.md", "planted", &bob)
            .await
            .expect_err("bob must not write alice's namespace")
            .contains("another operator"),
    );
}

/// A username still WINS over a subject when the proxy forwards both. That
/// order is the invariant, not a preference: a proxy that starts forwarding
/// `sub` beside a username it was already forwarding must not re-bucket the
/// slots already written under that username.
#[tokio::test]
async fn a_username_beside_a_subject_keeps_the_username_namespace() {
    let h = harness(true).await;
    let both = vec![
        ("authorization", "Bearer the-root-token"),
        ("x-memory-actor-proxy-secret", PROXY_SECRET),
        ("x-memory-actor-user", "alice"),
        ("x-memory-actor-sub", ALICE_SUB),
    ];

    assert_eq!(
        write_slot(&h.http, "_slots/current-focus.md", "alice only", &both)
            .await
            .expect("write"),
        "_slots/alice/current-focus.md",
        "forwarding a subject alongside the username moved the operator's slots",
    );
}

/// DEFAULT CONFIG (`[slots] per_user` off). The identity rule is never
/// consulted: every path lands as given and every slot is in every brief,
/// byte-identical to the pre-feature behaviour — including a nested path, which
/// carries no ownership meaning in this mode.
#[tokio::test]
async fn default_slot_config_is_unchanged() {
    let h = harness(false).await;
    let alice = proxied("x-memory-actor-sub", ALICE_SUB);
    let bob = proxied("x-memory-actor-sub", BOB_SUB);

    for path in ["_slots/current-focus.md", "_slots/alice/current-focus.md"] {
        assert_eq!(
            write_slot(&h.http, path, "body", &alice).await.expect(path),
            path,
            "with per-user slots off nothing may be re-homed or refused",
        );
    }
    let bobs = brief_slots(&h.http, &bob).await;
    for path in ["_slots/current-focus.md", "_slots/alice/current-focus.md"] {
        assert!(
            bobs.contains(&path.to_string()),
            "a slot vanished from an unrelated caller's brief: {bobs:?}",
        );
    }
}
