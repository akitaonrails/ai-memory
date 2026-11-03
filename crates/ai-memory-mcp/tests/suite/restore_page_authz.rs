//! `memory_restore_page` through the production auth stack.
//!
//! Git keeps every version the live wiki dropped: a page a root deleted, a
//! session purge removed, an expired page the forget sweep hard-deleted. The
//! tool's listing names those versions and a restore brings their text back,
//! so both carry the authority of root-only `/admin/restore-page`. These tests
//! drive the real `require_bearer` middleware with real `aim_` DB-user tokens
//! and the trusted-proxy rung, attempt each bypass, and keep a root control
//! beside every refusal so a blanket deny cannot pass for the guard.

use ai_memory_core::{ActorContext, ApiCredentialId, NewUser, ProjectId, UserRole};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_mcp::auth::{AuthState, require_bearer};
use ai_memory_store::{AccessMode, GrantLevel, Store, TokenPepper, generate_api_key, hash_token};
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
const PROXY_TOKEN: &str = "the-proxy-token";
const PEPPER: &str = "test-pepper-restore-page-authz-123456789";
const ISSUER: &str = "https://idp.example";
const ALICE_SUB: &str = "oidc-subject-alice";

struct Harness {
    http: Router,
    store: Store,
    shared: ProjectId,
    _tmp: TempDir,
}

/// A multi-user server: root bearer, `aim_` DB users, a trusted proxy, and
/// per-user slots on. `shared` is open to every user, `secret` is restricted.
async fn harness() -> Harness {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let baked = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .expect("baked");
    let shared = store
        .writer
        .get_or_create_project(ws, "shared", None)
        .await
        .expect("shared");
    let secret = store
        .writer
        .get_or_create_project(ws, "secret", None)
        .await
        .expect("secret");
    store
        .writer
        .set_access_mode(secret, AccessMode::Restricted)
        .await
        .expect("restrict secret");
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, baked)
        .with_wiki(wiki)
        .with_per_user_slots(true)
        .with_trusted_proxy_identity(true);
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let auth = AuthState::new(Some(ROOT_TOKEN.to_string()))
        .with_root_actor(ActorContext {
            user: Some("dj".to_string()),
            ..ActorContext::default()
        })
        .with_trusted_proxy_bearer(PROXY_TOKEN)
        .with_multiuser(
            TokenPepper::new(PEPPER),
            store.reader.clone(),
            store.writer.clone(),
        );
    let http =
        Router::new()
            .nest_service("/mcp", service)
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(auth),
                require_bearer,
            ));
    Harness {
        http,
        store,
        shared,
        _tmp: tmp,
    }
}

/// Mint a DB user with an `aim_` token; `write_on` grants write on a project.
async fn db_user(h: &Harness, username: &str, write_on: Option<ProjectId>) -> String {
    let token = generate_api_key().expect("token");
    let id = h
        .store
        .writer
        .create_human_user(
            NewUser {
                username: username.into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .expect("user");
    h.store
        .writer
        .create_api_credential(
            ApiCredentialId::new(),
            id,
            "test".into(),
            hash_token(&token, &TokenPepper::new(PEPPER)),
            None,
        )
        .await
        .expect("credential");
    if let Some(project) = write_on {
        h.store
            .writer
            .grant_memory(id, project, GrantLevel::Write, None)
            .await
            .expect("grant");
    }
    token
}

/// The raw JSON-RPC outcome: `Ok` is the tool's JSON payload, `Err` the
/// refusal message (JSON-RPC error or an `isError` tool result).
async fn call(
    router: &Router,
    bearer: &str,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    call_with(
        router,
        &[("authorization", &format!("Bearer {bearer}"))],
        name,
        arguments,
    )
    .await
}

async fn call_with(
    router: &Router,
    headers: &[(&str, &str)],
    name: &str,
    arguments: Value,
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
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .expect("body");
    let text = String::from_utf8(bytes.to_vec()).expect("utf8");
    if !status.is_success() {
        return Err(format!("HTTP {status}: {text}"));
    }
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
    if v.pointer("/result/isError").and_then(Value::as_bool) == Some(true) {
        return Err(joined);
    }
    Ok(serde_json::from_str(&joined)
        .unwrap_or_else(|e| panic!("tool text not JSON: {joined}: {e}")))
}

fn scoped(project: &str, extra: Value) -> Value {
    let mut args = json!({ "workspace": "default", "project": project });
    if let (Some(map), Value::Object(extra)) = (args.as_object_mut(), extra) {
        map.extend(extra);
    }
    args
}

/// Root writes then deletes `path` in `project`; returns the pre-delete rev
/// root's own listing names, so a refused caller is handed a valid rev.
async fn written_then_deleted(h: &Harness, project: &str, path: &str, body: &str) -> String {
    call(
        &h.http,
        ROOT_TOKEN,
        "memory_write_page",
        scoped(project, json!({ "path": path, "body": body })),
    )
    .await
    .expect("root write");
    call(
        &h.http,
        ROOT_TOKEN,
        "memory_delete_page",
        scoped(project, json!({ "path": path })),
    )
    .await
    .expect("root delete");
    pre_delete_rev(h, project, path).await
}

async fn pre_delete_rev(h: &Harness, project: &str, path: &str) -> String {
    let listed = call(
        &h.http,
        ROOT_TOKEN,
        "memory_restore_page",
        scoped(project, json!({ "path": path })),
    )
    .await
    .expect("root lists checkpoints");
    listed["checkpoints"][1]["rev"]
        .as_str()
        .unwrap_or_else(|| panic!("write and delete checkpoints: {listed}"))
        .to_owned()
}

async fn root_reads(h: &Harness, project: &str, path: &str) -> Option<String> {
    call(
        &h.http,
        ROOT_TOKEN,
        "memory_read_page",
        scoped(project, json!({ "path": path })),
    )
    .await
    .ok()
    .and_then(|page| page["body"].as_str().map(str::to_owned))
}

/// A DB user who may WRITE the project still cannot list its checkpoints or
/// restore a version a root deleted. Root, the control, can do both.
#[tokio::test]
async fn a_db_user_with_a_write_grant_cannot_list_or_restore() {
    let h = harness().await;
    let alice = db_user(&h, "alice", Some(h.shared)).await;
    let rev = written_then_deleted(
        &h,
        "shared",
        "notes/removed.md",
        "# Removed\n\nroot-deleted text",
    )
    .await;

    // Alice's grant is real: an ordinary write in the project succeeds.
    call(
        &h.http,
        &alice,
        "memory_write_page",
        scoped(
            "shared",
            json!({ "path": "notes/alice.md", "body": "# Alice\n\nmine" }),
        ),
    )
    .await
    .expect("a write grant lets alice write");

    let listed = call(
        &h.http,
        &alice,
        "memory_restore_page",
        scoped("shared", json!({ "path": "notes/removed.md" })),
    )
    .await;
    assert!(
        listed.is_err(),
        "a DB user must not list checkpoints: {listed:?}"
    );
    let restored = call(
        &h.http,
        &alice,
        "memory_restore_page",
        scoped("shared", json!({ "path": "notes/removed.md", "rev": rev })),
    )
    .await;
    assert!(
        restored.is_err(),
        "a DB user must not restore: {restored:?}"
    );
    assert_eq!(
        root_reads(&h, "shared", "notes/removed.md").await,
        None,
        "the refused restore must not have written anything"
    );

    // Control: root restores the same rev.
    let restored = call(
        &h.http,
        ROOT_TOKEN,
        "memory_restore_page",
        scoped("shared", json!({ "path": "notes/removed.md", "rev": rev })),
    )
    .await
    .expect("root restores");
    assert_eq!(restored["restored_from"], rev.as_str());
    assert!(
        root_reads(&h, "shared", "notes/removed.md")
            .await
            .is_some_and(|body| body.contains("root-deleted text"))
    );
}

/// A restricted project the caller holds no grant on: neither listing nor
/// restoring reaches it, while root's control succeeds. An unknown bearer is
/// refused at the door.
#[tokio::test]
async fn a_foreign_restricted_project_is_refused_without_a_grant() {
    let h = harness().await;
    let mallory = db_user(&h, "mallory", Some(h.shared)).await;
    let rev = written_then_deleted(&h, "secret", "notes/plan.md", "# Plan\n\nsecret text").await;

    for args in [
        scoped("secret", json!({ "path": "notes/plan.md" })),
        scoped("secret", json!({ "path": "notes/plan.md", "rev": rev })),
    ] {
        let out = call(&h.http, &mallory, "memory_restore_page", args.clone()).await;
        let refused = out.expect_err("a foreign restricted project must be refused");
        assert!(
            !refused.contains("secret text") && !refused.contains("Plan"),
            "the refusal must not leak the version: {refused}"
        );
        let out = call(&h.http, "aim_not-a-real-token", "memory_restore_page", args).await;
        assert!(out.is_err(), "an unknown bearer must be refused: {out:?}");
    }
    assert_eq!(root_reads(&h, "secret", "notes/plan.md").await, None);

    call(
        &h.http,
        ROOT_TOKEN,
        "memory_restore_page",
        scoped("secret", json!({ "path": "notes/plan.md", "rev": rev })),
    )
    .await
    .expect("root restores in the restricted project");
    assert!(
        root_reads(&h, "secret", "notes/plan.md")
            .await
            .is_some_and(|body| body.contains("secret text"))
    );
}

/// Per-user slots: an operator's slot write lands in their own namespace.
/// The operator cannot restore it through this tool (restore is admin-only,
/// even for their own slot). Root restores the exact namespaced path, and
/// it is not re-homed into root's namespace or onto the shared slot that
/// reaches every operator's brief.
#[tokio::test]
async fn a_personal_slot_restores_to_its_exact_path_and_only_for_root() {
    let h = harness().await;
    let alice = [
        ("authorization", "Bearer the-proxy-token"),
        ("x-memory-actor-issuer", ISSUER),
        ("x-memory-actor-sub", ALICE_SUB),
    ];
    let written = call_with(
        &h.http,
        &alice,
        "memory_write_page",
        scoped(
            "scratch",
            json!({ "path": "_slots/current-focus.md", "body": "alice focus" }),
        ),
    )
    .await
    .expect("alice writes her slot");
    let personal = written["path"].as_str().expect("written path").to_owned();
    assert_ne!(
        personal, "_slots/current-focus.md",
        "per-user slots re-home the write"
    );
    call_with(
        &h.http,
        &alice,
        "memory_delete_page",
        scoped("scratch", json!({ "path": "_slots/current-focus.md" })),
    )
    .await
    .expect("alice deletes her slot");
    let rev = pre_delete_rev(&h, "scratch", &personal).await;

    for path in ["_slots/current-focus.md", personal.as_str()] {
        let out = call_with(
            &h.http,
            &alice,
            "memory_restore_page",
            scoped("scratch", json!({ "path": path, "rev": rev })),
        )
        .await;
        assert!(
            out.is_err(),
            "a non-admin operator must not restore {path}: {out:?}"
        );
    }
    assert_eq!(root_reads(&h, "scratch", &personal).await, None);

    let restored = call(
        &h.http,
        ROOT_TOKEN,
        "memory_restore_page",
        scoped("scratch", json!({ "path": personal, "rev": rev })),
    )
    .await
    .expect("root restores the personal slot");
    assert_eq!(
        restored["path"],
        personal.as_str(),
        "restored to the exact path"
    );
    assert!(
        root_reads(&h, "scratch", &personal)
            .await
            .is_some_and(|body| body.contains("alice focus"))
    );
    assert_eq!(
        root_reads(&h, "scratch", "_slots/current-focus.md").await,
        None,
        "a restore must never land on the shared slot"
    );
}
