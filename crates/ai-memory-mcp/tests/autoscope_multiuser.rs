//! End-to-end test for `auto_scope.mode = per_actor` with the **production
//! auth chain**: real `users` table rows, hashed bearer tokens, and a
//! middleware that mirrors `ai_memory_cli::auth::require_bearer`'s Bearer
//! → hash → DB-lookup → `ActorContext.user = user.username` path.
//!
//! Why a replica and not the real one: `ai-memory-cli` is a binary-only
//! crate (no `lib.rs`), so its `auth` module isn't reachable from an
//! integration test in another crate. The slice that matters for
//! `auto_scope` is the (Bearer header → ActorContext.user) wiring, which
//! is short enough to replicate verbatim. Cookie / Basic-auth paths are
//! covered elsewhere and have no bearing on actor-key derivation.
//!
//! If `require_bearer` adds a new ActorContext field that affects the
//! per-actor key, the replica below must mirror it. The
//! `users.username -> ActorContext.user` invariant is the load-bearing
//! one: an autoscope regression that fed `user.id` (UUID) instead of
//! `user.username` into the actor key would split the per-actor map
//! across rungs, which is exactly what this test pins.

use ai_memory_core::{ActiveProject, ActiveProjectMode, ActorContext, NewUser, Tier};
use ai_memory_hooks::{
    DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT, HookState, ProjectCacheStore, SubagentSessionSet,
    hook_router,
};
use ai_memory_mcp::AiMemoryServer;
use ai_memory_store::{Store, TokenPepper, generate_token, hash_token};
use ai_memory_wiki::Wiki;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;

struct SeededUser {
    /// Kept for debugging panics and future assertions on the
    /// `ActorContext.user` path; the tests below probe by token, but
    /// retaining the username avoids re-deriving it from the index.
    #[allow(dead_code)]
    username: String,
    token: String,
    project_index: usize,
}

struct MultiUserHarness {
    router: Router,
    _store: Store,
    _tmp: TempDir,
    users: Vec<SeededUser>,
}

impl MultiUserHarness {
    /// Build a server with multi-user auth enabled, seed N users (each
    /// owning a distinct seeded project), and assemble the production-
    /// shaped Router (Bearer middleware → /mcp + /hook).
    async fn build(num_users: usize) -> Self {
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
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).expect("wiki");

        // Seed N users with hashed tokens + their own project + marker page.
        let pepper = TokenPepper::new("test-pepper-multiuser-1234567890abcdef");
        let mut users = Vec::with_capacity(num_users);
        for i in 1..=num_users {
            let username = format!("user{i}");
            let raw_token = generate_token().expect("gen token");
            let hash = hash_token(&raw_token, &pepper);
            store
                .writer
                .create_user(
                    NewUser {
                        username: username.clone(),
                        name: Some(format!("User {i}")),
                        email: Some(format!("user{i}@example.com")),
                    },
                    hash,
                )
                .await
                .expect("create user");

            // Each user "owns" a uniquely-named project + marker page; the
            // FTS5 tokenizer treats `_` as a TOKEN character, so the body
            // uses space-separated alphabetic words to stay searchable.
            let proj = store
                .writer
                .get_or_create_project(ws, format!("user{i}-proj"), None)
                .await
                .expect("seed proj");
            store
                .writer
                .upsert_page(ai_memory_core::NewPage {
                    workspace_id: ws,
                    project_id: proj,
                    path: ai_memory_core::PagePath::new("marker.md").expect("path"),
                    title: format!("Marker for user {i}"),
                    body: format!("multistream {tag}", tag = number_word(i)),
                    tier: Tier::Semantic,
                    frontmatter_json: serde_json::json!({}),
                    pinned: false,
                    links: Vec::new(),
                    author_id: None,
                })
                .await
                .expect("seed page");

            users.push(SeededUser {
                username,
                token: raw_token,
                project_index: i,
            });
        }

        let active_project = ActiveProject::with_config(
            ActiveProjectMode::PerActor,
            Duration::from_secs(60),
            ai_memory_core::DEFAULT_MAX_ENTRIES,
        );

        let server = AiMemoryServer::new(store.reader.clone(), store.writer.clone(), ws, baked)
            .with_wiki(wiki.clone())
            .with_active_project(active_project.clone());

        let mcp_service = StreamableHttpService::new(
            move || Ok(server.clone()),
            LocalSessionManager::default().into(),
            StreamableHttpServerConfig::default()
                .with_stateful_mode(false)
                .with_json_response(true),
        );

        let project_cache: ai_memory_hooks::ProjectCache =
            Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default()));
        let hooks = hook_router(HookState {
            workspace_id: ws,
            project_id: baked,
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki: wiki.clone(),
            consolidator: None,
            sanitizer: ai_memory_core::Sanitizer::default(),
            project_cache,
            active_project: active_project.clone(),
            ingest_semaphore: Arc::new(tokio::sync::Semaphore::new(
                DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT,
            )),
            ingest_gates: ai_memory_hooks::IngestGates::default(),
            consolidate_on_session_end: false,
            capture_assistant_enabled: false,
            subagent_sessions: Arc::new(tokio::sync::Mutex::new(SubagentSessionSet::default())),
            ingest_rate: Arc::new(tokio::sync::Mutex::new(
                ai_memory_hooks::IngestRateLimiter::disabled(),
            )),
            home_dir: None,
        });

        // Production-equivalent Bearer middleware. Pepper + reader are
        // shared via a small Arc state. The implementation mirrors the
        // Bearer-only branch of `ai_memory_cli::auth::require_bearer`:
        // valid token → ActorContext { user: Some(username), name, email }.
        let auth_state = Arc::new(BearerAuthState {
            pepper,
            reader: store.reader.clone(),
        });
        let router = Router::new()
            .nest_service("/mcp", mcp_service)
            .merge(hooks)
            .layer(axum::middleware::from_fn_with_state(
                auth_state,
                bearer_lookup,
            ));

        MultiUserHarness {
            router,
            _store: store,
            _tmp: tmp,
            users,
        }
    }
}

#[derive(Clone)]
struct BearerAuthState {
    pepper: TokenPepper,
    reader: ai_memory_store::ReaderPool,
}

async fn bearer_lookup(
    axum::extract::State(state): axum::extract::State<Arc<BearerAuthState>>,
    mut req: Request<Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // No token → anonymous. This mirrors require_bearer's rung 0/
    // unrecognised-Bearer behavior for our autoscope test:
    // ActorContext is injected with no `user`, so PerActor must not match
    // any authenticated user's keyed slot.
    let bearer = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty());

    if let Some(token) = bearer {
        let hash = hash_token(token, &state.pepper);
        match state.reader.find_active_user_by_token_hash(hash).await {
            Ok(Some(user)) => {
                // Mirror require_bearer:
                //   ActorContext.user = user.username
                //   ActorContext.name  = user.name
                //   ActorContext.email = user.email
                req.extensions_mut().insert(ActorContext {
                    user: Some(user.username.clone()),
                    name: user.name.clone(),
                    email: user.email.clone(),
                    ..ActorContext::default()
                });
                req.extensions_mut().insert(user.id);
            }
            _ => {
                req.extensions_mut().insert(ActorContext::anonymous());
            }
        }
    } else {
        req.extensions_mut().insert(ActorContext::anonymous());
    }
    next.run(req).await
}

fn number_word(i: usize) -> &'static str {
    match i {
        1 => "alphaone",
        2 => "alphatwo",
        3 => "alphathree",
        4 => "alphafour",
        _ => panic!("number_word: 1..=4 only"),
    }
}

fn hook_request(token: &str, session_id: &str, project_name: &str) -> Request<Body> {
    let body = json!({
        "event": "session-start",
        "session_id": session_id,
        "cwd": format!("/tmp/aim-test/{project_name}"),
    });
    Request::builder()
        .method("POST")
        .uri(format!(
            "/hook?event=session-start&agent=claude-code&project={project_name}"
        ))
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .expect("hook req")
}

fn mcp_query_request(token: &str, session_id: &str) -> Request<Body> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "memory_query",
            "arguments": { "query": "multistream", "limit": 20 }
        }
    });
    Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "localhost")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", format!("Bearer {token}"))
        .header("x-memory-actor-session-id", session_id)
        .body(Body::from(body.to_string()))
        .expect("mcp req")
}

async fn response_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 4_000_000)
        .await
        .expect("body bytes");
    String::from_utf8(bytes.to_vec()).expect("utf8")
}

fn tool_text(body: &str) -> String {
    let v: serde_json::Value =
        serde_json::from_str(body).unwrap_or_else(|e| panic!("non-JSON: {body}\nerr: {e}"));
    if let Some(err) = v.get("error") {
        panic!("JSON-RPC error: {err}\nfull: {body}");
    }
    v.pointer("/result/content")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("missing result.content: {body}"))
        .iter()
        .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn fire_hook_and_settle(router: &Router, token: &str, session_id: &str, proj: &str) {
    let resp = router
        .clone()
        .oneshot(hook_request(token, session_id, proj))
        .await
        .expect("hook oneshot");
    assert_eq!(resp.status(), StatusCode::ACCEPTED, "hook must accept");
    let _ = response_text(resp).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(15)).await;
}

// ────────────────────────────────────────────────────────────────────────────
// Each user, authenticated by their real Bearer token, hooks their own
// project; concurrent reads must resolve into their OWN slot, never any
// sibling user's, even though they all share the same `session_id` value.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn per_actor_isolates_real_bearer_users() {
    let h = MultiUserHarness::build(3).await;

    // Intentionally shared session id across all users: the per_actor map
    // must key by `(user, session)`, so even with collisions on the
    // session-id half, the user-half from the auth middleware partitions
    // them correctly. If a regression dropped `user` from the actor key,
    // ALL three users would collapse into one slot and the assertion
    // below would fail.
    let shared_sess = "shared-session-across-users";
    for u in &h.users {
        let proj = format!("user{}-proj", u.project_index);
        fire_hook_and_settle(&h.router, &u.token, shared_sess, &proj).await;
    }

    // Concurrent reads, one per user, with the shared session id.
    let mut handles = Vec::new();
    for u in &h.users {
        let router = h.router.clone();
        let token = u.token.clone();
        let i = u.project_index;
        handles.push(tokio::spawn(async move {
            let resp = router
                .oneshot(mcp_query_request(&token, shared_sess))
                .await
                .expect("mcp oneshot");
            assert_eq!(resp.status(), StatusCode::OK, "user{i} read status");
            let body = response_text(resp).await;
            (i, tool_text(&body))
        }));
    }

    for hd in handles {
        let (i, text) = hd.await.expect("join");
        let own = number_word(i);
        assert!(
            text.contains(own),
            "user{i} did NOT see its own marker. Expected {own:?}:\n{text}"
        );
        for j in 1..=3 {
            if j == i {
                continue;
            }
            let foreign = number_word(j);
            assert!(
                !text.contains(foreign),
                "user{i} leaked into user{j}'s project. Saw {foreign:?}:\n{text}"
            );
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// A request without a Bearer token authenticates as anonymous → no
// `user` in the actor key. If it still carries a session id, PerActor
// must fail closed rather than falling through to the latest authenticated
// user's slot. The per-actor map must NOT serve user1's pointer to an
// anonymous request, because the keys `(user1, sess)` and `(None, sess)`
// are distinct.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anonymous_request_does_not_inherit_user_slot() {
    let h = MultiUserHarness::build(2).await;
    let sess = "anon-vs-user1";

    let user1 = &h.users[0]; // owns user1-proj (alphaone marker)

    // user1 publishes their per_actor slot.
    fire_hook_and_settle(&h.router, &user1.token, sess, "user1-proj").await;

    // Anonymous read with the SAME session id. Because `user` differs
    // (`None` vs `Some("user1")`), the per_actor slot is a different
    // key, and the slot is empty for anonymous. That must return the baked
    // default rather than user1's latest hook-published project.
    let resp = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "localhost")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("x-memory-actor-session-id", sess)
                // NO authorization header.
                .body(Body::from(
                    json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "tools/call",
                        "params": {
                            "name": "memory_query",
                            "arguments": { "query": "multistream", "limit": 20 }
                        }
                    })
                    .to_string(),
                ))
                .expect("anon mcp req"),
        )
        .await
        .expect("anon mcp oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    // Body must parse cleanly; engine must not crash on anonymous +
    // per_actor mode, and must not leak user1's marker via fallback.
    let text = tool_text(&response_text(resp).await);
    assert!(
        !text.contains(number_word(user1.project_index)),
        "anonymous request leaked user1 marker via fallback:\n{text}"
    );
}

// ────────────────────────────────────────────────────────────────────────────
// A request with an unknown Bearer (typo, revoked token) must also fall
// through to anonymous, not get spoofed into another user's slot. This
// pins the rung-2 lookup-miss path.
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_bearer_does_not_match_any_user_slot() {
    let h = MultiUserHarness::build(2).await;
    let sess = "unknown-bearer-probe";
    let user1 = &h.users[0];

    fire_hook_and_settle(&h.router, &user1.token, sess, "user1-proj").await;

    // Forged Bearer of the right shape but unknown to the users table.
    let forged = "deadbeef".repeat(8);
    let resp = h
        .router
        .clone()
        .oneshot(mcp_query_request(&forged, sess))
        .await
        .expect("forged mcp oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_text(resp).await;
    let text = tool_text(&body);

    // The forged caller's key is `(None, sess)` after the middleware's
    // anonymous fallback. It must NOT key into user1's `(Some("user1"),
    // sess)` slot or fall through to user1's latest project.
    assert!(
        !text.contains(number_word(user1.project_index)),
        "unknown bearer request leaked user1 marker via fallback:\n{text}"
    );
}
