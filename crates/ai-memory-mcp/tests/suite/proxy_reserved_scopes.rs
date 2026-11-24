//! Adversarial tests for `GHSA-v4vw-rf7v-cxgf`: non-root trusted-proxy callers
//! must be refused writes/mutations to reserved shared scopes (`default/_global`,
//! `<ws>/_profile`) and private profile scopes (`_profile.<user_id>`), while
//! keeping reads on shared scopes and writes on ordinary open projects, and
//! admitting root-matched proxy identities and the static root bearer.

use std::sync::Arc;

use ai_memory_core::profile::{
    EffectiveProfileShare, ProfileEnabled, ProfileSettings, ProfileShare,
    WORKSPACE_PROFILE_PROJECT, user_profile_project,
};
use ai_memory_core::{ActorContext, NewUser, UserRole};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_mcp::auth::{AuthState, require_bearer};
use ai_memory_mcp::messages::{self, MessageState};
use ai_memory_store::{Store, create_global_scope, create_profile_scope};
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

const ROOT_TOKEN: &str = "the-root-token";
const PROXY_TOKEN: &str = "the-proxy-token";
const ISSUER: &str = "https://idp.example";
const ROOT_SUB: &str = "oidc-subject-root";
const BOB_SUB: &str = "oidc-subject-bob";

async fn try_mcp_call(
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
        return Err(err["message"].as_str().unwrap_or(&text).to_string());
    }
    if v["result"]["isError"].as_bool() == Some(true) {
        let msg = v["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or(&text)
            .to_string();
        return Err(msg);
    }
    Ok(v["result"].clone())
}

#[tokio::test]
async fn non_root_proxy_user_cannot_mutate_global_or_profile_scopes() {
    let tmp = TempDir::new().expect("tempdir");
    let store = Store::open(tmp.path()).expect("store");
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .expect("ws");
    let scratch = store
        .writer
        .get_or_create_project(ws, "scratch", None)
        .await
        .expect("scratch");
    let _global = create_global_scope(&store.writer).await.expect("global");
    let _ws_profile = store
        .writer
        .get_or_create_project(ws, WORKSPACE_PROFILE_PROJECT, None)
        .await
        .expect("ws_profile");
    let alice = store
        .writer
        .create_human_user(
            NewUser {
                username: "alice".into(),
                name: None,
                email: None,
            },
            UserRole::User,
            None,
            false,
        )
        .await
        .expect("alice");
    let _alice_profile = create_profile_scope(
        &store.reader,
        &store.writer,
        EffectiveProfileShare::User,
        ws,
        Some(alice),
    )
    .await
    .expect("alice_profile");
    let alice_profile_name = user_profile_project(alice);

    let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");
    let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, scratch)
        .with_wiki(wiki.clone())
        .with_trusted_proxy_identity(true)
        .with_profile(ProfileSettings {
            enabled: ProfileEnabled::On,
            share: ProfileShare::Workspace,
            ..ProfileSettings::default()
        });

    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let msg_router = messages::router(MessageState {
        reader: store.reader.clone(),
        writer: store.writer.clone(),
        wiki,
        trusted_proxy_identity: true,
    });

    let auth_state = AuthState::new(Some(ROOT_TOKEN.to_string()))
        .with_root_actor(ActorContext {
            issuer: Some(ISSUER.to_string()),
            sub: Some(ROOT_SUB.to_string()),
            ..ActorContext::default()
        })
        .with_trusted_proxy_bearer(PROXY_TOKEN);

    let router = Router::new()
        .nest_service("/mcp", service)
        .merge(msg_router)
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(auth_state),
            require_bearer,
        ));

    let root_bearer_headers = [("authorization", format!("Bearer {ROOT_TOKEN}"))];
    let root_bearer: Vec<(&str, &str)> = root_bearer_headers
        .iter()
        .map(|(k, v)| (*k, v.as_str()))
        .collect();

    let proxy_auth = format!("Bearer {PROXY_TOKEN}");
    let bob_proxy = [
        ("authorization", proxy_auth.as_str()),
        ("x-memory-actor-issuer", ISSUER),
        ("x-memory-actor-sub", BOB_SUB),
    ];
    let root_proxy = [
        ("authorization", proxy_auth.as_str()),
        ("x-memory-actor-issuer", ISSUER),
        ("x-memory-actor-sub", ROOT_SUB),
    ];

    // Seed readable pages in default/_global and default/_profile as static root.
    let seeded_global = try_mcp_call(
        &router,
        "memory_write_page",
        json!({
            "scope": "global",
            "path": "prefs/style.md",
            "title": "Style",
            "body": "Use explicit types everywhere.",
        }),
        &root_bearer,
    )
    .await
    .expect("root writes global seed");
    let seeded_text = seeded_global["content"][0]["text"]
        .as_str()
        .expect("seed text");
    let seeded_json: Value = serde_json::from_str(seeded_text).expect("seed json");
    let global_page_id = seeded_json["page_id"]
        .as_str()
        .expect("page_id")
        .to_string();

    try_mcp_call(
        &router,
        "memory_write_page",
        json!({
            "workspace": "default",
            "project": WORKSPACE_PROFILE_PROJECT,
            "path": "profile/team.md",
            "title": "Team defaults",
            "body": "Run clippy before pushing.",
        }),
        &root_bearer,
    )
    .await
    .expect("root writes workspace profile seed");

    // --- Adversarial cases: non-root proxied user is refused every mutation ---
    let write_attempts: Vec<(&str, Value)> = vec![
        (
            "scope=global",
            json!({
                "scope": "global",
                "path": "prefs/hijack.md",
                "title": "Hijack",
                "body": "malicious global default",
            }),
        ),
        (
            "explicit default/_global",
            json!({
                "workspace": "default",
                "project": "_global",
                "path": "prefs/hijack2.md",
                "title": "Hijack 2",
                "body": "malicious global default",
            }),
        ),
        (
            "explicit default/_profile",
            json!({
                "workspace": "default",
                "project": WORKSPACE_PROFILE_PROJECT,
                "path": "profile/hijack.md",
                "title": "Hijack profile",
                "body": "malicious workspace profile",
            }),
        ),
        (
            "scope=profile",
            json!({
                "scope": "profile",
                "path": "profile/hijack-scope.md",
                "title": "Hijack scope profile",
                "body": "malicious profile write",
            }),
        ),
        (
            "explicit private profile _profile.<alice>",
            json!({
                "workspace": "default",
                "project": alice_profile_name,
                "path": "profile/squat.md",
                "title": "Squat",
                "body": "malicious private profile write",
            }),
        ),
    ];
    for (label, args) in write_attempts {
        let err = try_mcp_call(&router, "memory_write_page", args, &bob_proxy)
            .await
            .expect_err(&format!("non-root proxy must be refused write ({label})"));
        assert!(
            err.to_lowercase().contains("not authorized")
                || err.to_lowercase().contains("reserved")
                || err.to_lowercase().contains("global scope")
                || err.to_lowercase().contains("profile"),
            "unexpected refusal message for {label}: {err}"
        );
    }

    // memory_patch_page on default/_global (both scope="global" and explicit)
    for (label, args) in [
        (
            "patch scope=global",
            json!({
                "scope": "global",
                "path": "prefs/style.md",
                "expected_page_id": global_page_id,
                "edits": [{ "old_text": "explicit", "new_text": "poisoned" }],
            }),
        ),
        (
            "patch explicit default/_global",
            json!({
                "workspace": "default",
                "project": "_global",
                "path": "prefs/style.md",
                "expected_page_id": global_page_id,
                "edits": [{ "old_text": "explicit", "new_text": "poisoned" }],
            }),
        ),
    ] {
        try_mcp_call(&router, "memory_patch_page", args, &bob_proxy)
            .await
            .expect_err(&format!("non-root proxy must be refused {label}"));
    }

    // memory_write_pages_batch on default/_global
    try_mcp_call(
        &router,
        "memory_write_pages_batch",
        json!({
            "workspace": "default",
            "project": "_global",
            "pages": [{
                "path": "prefs/batch.md",
                "title": "Batch",
                "body": "poisoned batch",
            }],
        }),
        &bob_proxy,
    )
    .await
    .expect_err("non-root proxy must be refused memory_write_pages_batch on default/_global");

    // memory_delete_page on default/_global
    try_mcp_call(
        &router,
        "memory_delete_page",
        json!({
            "workspace": "default",
            "project": "_global",
            "path": "prefs/style.md",
        }),
        &bob_proxy,
    )
    .await
    .expect_err("non-root proxy must be refused memory_delete_page on default/_global");

    // memory_resolve_write_target on default/_global
    try_mcp_call(
        &router,
        "memory_resolve_write_target",
        json!({
            "workspace": "default",
            "project": "_global",
        }),
        &bob_proxy,
    )
    .await
    .expect_err("non-root proxy must be refused memory_resolve_write_target on default/_global");

    // POST /messages/send targeting default/_global -> 403 Forbidden
    let msg_req = Request::builder()
        .method("POST")
        .uri("/messages/send")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("authorization", proxy_auth.as_str())
        .header("x-memory-actor-issuer", ISSUER)
        .header("x-memory-actor-sub", BOB_SUB)
        .body(Body::from(
            json!({
                "from_workspace": "default",
                "from_project": "scratch",
                "to_workspace": "default",
                "to_project": "_global",
                "body": "poisoned broadcast message",
            })
            .to_string(),
        ))
        .expect("msg req");
    let msg_resp = router.clone().oneshot(msg_req).await.expect("oneshot");
    assert_eq!(
        msg_resp.status(),
        StatusCode::FORBIDDEN,
        "non-root proxy must get 403 on POST /messages/send to default/_global"
    );

    // --- Legitimate controls ---
    // 1. Non-root proxy can read default/_global and default/_profile
    try_mcp_call(
        &router,
        "memory_read_page",
        json!({
            "scope": "global",
            "path": "prefs/style.md",
        }),
        &bob_proxy,
    )
    .await
    .expect("control: non-root proxy reads default/_global");

    try_mcp_call(
        &router,
        "memory_read_page",
        json!({
            "workspace": "default",
            "project": WORKSPACE_PROFILE_PROJECT,
            "path": "profile/team.md",
        }),
        &bob_proxy,
    )
    .await
    .expect("control: non-root proxy reads default/_profile");

    // 2. Non-root proxy can write an ordinary open project
    try_mcp_call(
        &router,
        "memory_write_page",
        json!({
            "workspace": "default",
            "project": "scratch",
            "path": "notes/bob.md",
            "title": "Bob note",
            "body": "Ordinary project write succeeds.",
        }),
        &bob_proxy,
    )
    .await
    .expect("control: non-root proxy writes ordinary open project");

    // 3. Root-matched proxy identity can write default/_global and default/_profile
    try_mcp_call(
        &router,
        "memory_write_page",
        json!({
            "scope": "global",
            "path": "prefs/root-proxy.md",
            "title": "Root proxy global",
            "body": "Root-matched proxy identity may write _global.",
        }),
        &root_proxy,
    )
    .await
    .expect("control: root-matched proxy writes _global");

    try_mcp_call(
        &router,
        "memory_write_page",
        json!({
            "workspace": "default",
            "project": WORKSPACE_PROFILE_PROJECT,
            "path": "profile/root-proxy.md",
            "title": "Root proxy profile",
            "body": "Root-matched proxy identity may write _profile.",
        }),
        &root_proxy,
    )
    .await
    .expect("control: root-matched proxy writes _profile");

    // 4. Static root bearer can write default/_global and default/_profile
    try_mcp_call(
        &router,
        "memory_write_page",
        json!({
            "scope": "global",
            "path": "prefs/static-root.md",
            "title": "Static root global",
            "body": "Static root bearer may write _global.",
        }),
        &root_bearer,
    )
    .await
    .expect("control: static root bearer writes _global");

    try_mcp_call(
        &router,
        "memory_write_page",
        json!({
            "workspace": "default",
            "project": WORKSPACE_PROFILE_PROJECT,
            "path": "profile/static-root.md",
            "title": "Static root profile",
            "body": "Static root bearer may write _profile.",
        }),
        &root_bearer,
    )
    .await
    .expect("control: static root bearer writes _profile");
}
