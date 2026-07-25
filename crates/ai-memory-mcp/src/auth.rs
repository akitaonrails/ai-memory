//! Authorization middleware for the HTTP server.
//!
//! When `[auth].bearer_token` (or the `AI_MEMORY_AUTH_TOKEN` env var)
//! is set, every request to `/mcp`, `/hook`, `/handoff`, and `/web/*`
//! must present the token via one of three transports:
//!
//! - **Bearer header** (any method): MCP clients + hooks. Required
//!   on all non-GET methods.
//! - **Basic auth** (GET only): browsers — username ignored, token
//!   in the password field. Triggers the native credential dialog
//!   via the `WWW-Authenticate: Basic` challenge in 401 responses.
//! - **Session cookie** (GET only): set automatically after a
//!   successful Basic auth so the browser doesn't re-prompt every
//!   session.
//!
//! When the token is *unset*, the middleware is a no-op — preserving
//! the zero-config local-development experience and keeping the
//! existing e2e + unit tests working.
//!
//! Comparison uses [`subtle::ConstantTimeEq`] so an attacker on the
//! same LAN cannot use response-time leaks to recover the token byte
//! by byte. The constant-time guarantee depends on both sides being
//! the same length; `subtle` returns a constant-cost `Choice::from(0)`
//! when lengths differ, which is the right thing here.
//!
//! Wire shape matches the MCP authorization spec
//! (modelcontextprotocol.io/specification/.../basic/authorization):
//! 401 responses include `WWW-Authenticate: Bearer …` so MCP clients
//! detect missing/expired credentials. GET 401s ALSO include `Basic
//! …` so browsers dialog-prompt automatically.
//!
//! ## Why not OAuth
//!
//! The MCP spec mandates full OAuth 2.1 for HTTP-authenticated
//! servers. That's overkill for a single-user homelab and would
//! force every MCP client config to deal with authorization-server
//! discovery + PKCE + token refresh. A static bearer token is
//! wire-compatible with the spec's `Authorization: Bearer …` shape
//! (clients send the header the same way; they just don't run the
//! OAuth dance to obtain the token). Every supported client
//! (Claude Code, Codex, OpenCode, Cursor, Claude Desktop via
//! `mcp-remote`, Gemini CLI, OpenClaw) accepts a static
//! `Authorization` header in its config.

use std::sync::Arc;

use ai_memory_core::{ActorContext, AuthLevel};
use ai_memory_store::{ReaderPool, TokenPepper, WriterHandle, hash_token};
use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tracing::debug;

/// Cookie name used for browser session persistence after a
/// successful Basic auth handshake.
const AUTH_COOKIE: &str = "ai_memory_auth";
/// Realm advertised in `WWW-Authenticate` challenges. Shows up in
/// the browser's credential prompt as "Server says: <realm>".
const AUTH_REALM: &str = "ai-memory";

/// Optional multi-user resolver tier — token-hash lookup against the
/// `users` table. Populated only when both a per-server pepper and a
/// reader pool are available (i.e. after `ai-memory init` ran and a
/// store was opened). Single-user (rung-1) setups skip this entirely;
/// rung-0 (no auth) skips the whole middleware.
#[derive(Clone)]
pub struct MultiUserResolver {
    /// Hashes incoming tokens with the per-server pepper before the
    /// `users.token_hash` lookup.
    pub pepper: TokenPepper,
    /// Read-only pool used by the auth hot path
    /// (`find_active_user_by_token_hash`).
    pub reader: ReaderPool,
    /// Writer used by the fire-and-forget `last_seen_at` bump after a
    /// successful lookup — kept off the response's critical path via
    /// `tokio::spawn`. The bump is best-effort: an error is logged at
    /// `warn` and otherwise ignored.
    pub writer: WriterHandle,
}

/// Shared auth state. Cheap to clone — just an `Arc` wrapping the
/// optional configured token + the optional multi-user resolver +
/// the root actor template.
#[derive(Clone, Default)]
pub struct AuthState {
    /// Bearer token that authenticates as **root**. `None` means
    /// "auth disabled at the wire level" — rung 0.
    expected: Option<String>,
    /// Actor template stamped onto requests that authenticate with the
    /// `expected` token. Populated from `[auth].root_username` /
    /// `root_email` / `root_name`. When all three are unset the root
    /// actor stays anonymous (rung-1 backward-compat: bearer
    /// authenticates but the audit log records nothing identifying).
    root_actor: ActorContext,
    /// Multi-user lookup tier. `None` until both pepper and reader
    /// are available — see [`Self::with_multiuser`].
    multiuser: Option<MultiUserResolver>,
}

impl AuthState {
    /// Build state from the (optional) configured root token. `None`
    /// means "auth disabled, accept everything as anonymous".
    #[must_use]
    pub fn new(expected: Option<String>) -> Self {
        Self {
            // A blank configured value must never turn an absent credential
            // into root authentication (`"" == ""`). Treat placeholders and
            // whitespace-only environment values as auth-disabled instead.
            expected: expected.filter(|token| !token.trim().is_empty()),
            ..Self::default()
        }
    }

    /// Attach the root actor template — see [`Self::root_actor`]. The
    /// auth middleware injects this on every request that authenticates
    /// with [`Self::expected`]; rung-2 (DB user) lookups override it
    /// with the row's identity, rung-0 (anonymous) leaves it empty.
    #[must_use]
    pub fn with_root_actor(mut self, actor: ActorContext) -> Self {
        self.root_actor = actor;
        self
    }

    /// Enable multi-user lookups: a bearer that doesn't match the root
    /// token is hashed with `pepper` and checked against the `users`
    /// table; a hit attributes the request to that user's identity.
    /// Without this attach, the middleware only knows about rung 0/1
    /// and rejects unknown bearers (closing the bypass).
    #[must_use]
    pub fn with_multiuser(
        mut self,
        pepper: TokenPepper,
        reader: ReaderPool,
        writer: WriterHandle,
    ) -> Self {
        self.multiuser = Some(MultiUserResolver {
            pepper,
            reader,
            writer,
        });
        self
    }

    /// True when a token is configured (i.e. the middleware is doing
    /// anything). Useful for the startup log line so the operator
    /// sees whether their server is open or closed.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.expected.is_some()
    }
}

/// axum middleware closure. Wire with
/// `axum::middleware::from_fn_with_state(state, require_bearer)`.
///
/// Token sources, in priority order:
/// 1. `Authorization: Bearer <token>` header. Works for any method.
///    This is what MCP + hook clients send.
/// 2. **GET only:** `Authorization: Basic <base64(user:token)>`.
///    Username is ignored; the password portion is the token.
///    Browsers send this automatically after the native credential
///    prompt fires on a 401 + `WWW-Authenticate: Basic`. On success
///    we also set the `ai_memory_auth` cookie so subsequent visits
///    (including from a fresh browser session) skip the prompt.
/// 3. **GET only:** `ai_memory_auth` cookie set by the Basic handshake.
///
/// POST / PUT / DELETE / etc. require the Bearer header. Cookie and
/// Basic auth are GET-only, which confines cookie-CSRF to read-only
/// pages — `/mcp` + `/hook` are POST-only and stay header-gated.
///
/// On 401 for GET requests the response includes both `Basic` and
/// `Bearer` challenges in `WWW-Authenticate`. Browsers honour the
/// `Basic` challenge (native dialog); MCP clients honour the `Bearer`
/// challenge.
pub async fn require_bearer(
    State(state): State<Arc<AuthState>>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    // Rung 0: auth disabled. Inject anonymous actor + anonymous
    // tier (so downstream handlers that read Extension<ActorContext>
    // and Extension<AuthLevel> always have one), then pass through.
    let Some(expected) = state.expected.as_deref() else {
        req.extensions_mut().insert(ActorContext::anonymous());
        req.extensions_mut().insert(AuthLevel::Anonymous);
        return next.run(req).await;
    };

    let is_get = req.method() == Method::GET;
    let from_bearer = extract_bearer_header(&req);
    let from_basic = if is_get {
        extract_basic_header(&req)
    } else {
        None
    };
    let from_cookie = if is_get { extract_cookie(&req) } else { None };

    let provided = from_bearer
        .as_deref()
        .or(from_basic.as_deref())
        .or(from_cookie.as_deref())
        .unwrap_or("");

    // Rung 1: bearer matches the root token → attribute as root.
    if bool::from(provided.as_bytes().ct_eq(expected.as_bytes())) {
        let mut actor = state.root_actor.clone();
        // The agent field comes from the request layer (MCP client
        // info, hook payload) — not from config. Leave it for the
        // hook router / MCP server to overlay onto the actor.
        actor.agent = actor.agent.or(None);
        req.extensions_mut().insert(actor);
        req.extensions_mut().insert(AuthLevel::Root);

        // First successful Basic-auth hit (no cookie yet) → also stamp
        // the cookie so the user doesn't get the dialog again next
        // browser session. Subsequent navigations ride the cookie alone.
        if from_basic.is_some() && from_cookie.is_none() {
            let mut resp = next.run(req).await;
            if let Ok(cookie) = build_session_cookie(provided).parse() {
                resp.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            return resp;
        }
        return next.run(req).await;
    }

    // Rung 2: bearer doesn't match root. If multi-user is enabled,
    // hash + look up the token against the `users` table.
    if let Some(mu) = state.multiuser.as_ref()
        && !provided.is_empty()
    {
        let hash = hash_token(provided, &mu.pepper);
        match mu.reader.find_active_user_by_token_hash(hash).await {
            Ok(Some(user)) => {
                // NEVER log the token itself; the username + agent is
                // safe and useful for "who hit /api/v1 last".
                debug!(actor.user = %user.username, "authenticated as DB user");
                let actor = ActorContext {
                    user: Some(user.username.clone()),
                    name: user.name.clone(),
                    email: user.email.clone(),
                    ..ActorContext::default()
                };
                req.extensions_mut().insert(actor);
                req.extensions_mut().insert(user.id);
                req.extensions_mut().insert(AuthLevel::User);

                // Fire-and-forget last_seen_at bump. Errors are logged
                // but never block the response — middleware MUST stay
                // off the response's critical path. Same browser-cookie
                // dance as rung 1 above.
                let writer = mu.writer.clone();
                let user_id = user.id;
                tokio::spawn(async move {
                    if let Err(e) = writer.touch_user_last_seen(user_id).await {
                        tracing::warn!(error = %e, user_id = %user_id, "touch_user_last_seen failed");
                    }
                });

                if from_basic.is_some() && from_cookie.is_none() {
                    let mut resp = next.run(req).await;
                    if let Ok(cookie) = build_session_cookie(provided).parse() {
                        resp.headers_mut().insert(header::SET_COOKIE, cookie);
                    }
                    return resp;
                }
                return next.run(req).await;
            }
            Ok(None) => {
                // Bearer present + multi-user enabled + no match → fall
                // through to the 401 below. Critical for closing the
                // bypass: an unknown bearer MUST NOT pass even when
                // multi-user lookup is configured.
            }
            Err(e) => {
                tracing::error!(error = %e, "auth: users table lookup failed");
                return unauthorized(is_get);
            }
        }
    }

    debug!("auth rejected: invalid or missing token");
    unauthorized(is_get)
}

fn extract_bearer_header(req: &Request<axum::body::Body>) -> Option<String> {
    let h = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    // Accept both "Bearer xxx" and "bearer xxx" (case-insensitive
    // scheme per RFC 7235 §2.1).
    let (scheme, value) = h.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("Bearer") {
        Some(value.trim_start().to_string())
    } else {
        None
    }
}

fn extract_basic_header(req: &Request<axum::body::Body>) -> Option<String> {
    use base64::Engine;
    let h = req.headers().get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, value) = h.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value.trim_start())
        .ok()?;
    let s = std::str::from_utf8(&decoded).ok()?;
    // Standard form: `user:password`. We ignore the username (the
    // browser dialog always asks for one but we don't have multi-user
    // accounts — only the password = bearer token matters).
    let (_user, pass) = s.split_once(':')?;
    Some(pass.to_string())
}

fn extract_cookie(req: &Request<axum::body::Body>) -> Option<String> {
    let h = req.headers().get(header::COOKIE)?.to_str().ok()?;
    for pair in h.split(';') {
        let pair = pair.trim();
        if let Some(val) = pair.strip_prefix(&format!("{AUTH_COOKIE}=")) {
            return Some(val.to_string());
        }
    }
    None
}

fn build_session_cookie(token: &str) -> String {
    // 30-day Max-Age — long enough that re-entering the credential
    // every month is rare. HttpOnly hides it from any inline JS;
    // SameSite=Lax keeps cross-site POSTs from riding it.
    // No Secure attribute: homelab deployments are often plain HTTP
    // on a LAN. A TLS-terminating reverse proxy is the right place to
    // add Secure if the service is exposed publicly.
    format!("{AUTH_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/; Max-Age=2592000")
}

fn unauthorized(include_basic_challenge: bool) -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, "auth required\n").into_response();
    // Order of challenges matters: browsers parse the first challenge
    // they understand and show the dialog for it. Putting `Basic`
    // first ensures GET-from-browser triggers the native prompt; MCP
    // clients (which speak only Bearer) ignore the Basic and read
    // their challenge from the second value.
    //
    // Non-GET 401s skip the Basic challenge — sending it on a POST
    // would invite the browser to dialog-prompt for an endpoint
    // it can't authenticate this way anyway.
    let value = if include_basic_challenge {
        format!(
            "Basic realm=\"{AUTH_REALM}\", \
             Bearer realm=\"{AUTH_REALM}\", error=\"invalid_token\""
        )
    } else {
        format!("Bearer realm=\"{AUTH_REALM}\", error=\"invalid_token\"")
    };
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        value.parse().expect("static header value is valid"),
    );
    resp
}

/// Generate a fresh random bearer token, hex-encoded.
///
/// `bytes` is the entropy budget; 32 bytes (256 bits) is plenty for
/// any conceivable threat model.
///
/// # Errors
/// Propagates failures from the OS RNG.
pub fn generate_token_hex(bytes: usize) -> Result<String, getrandom::Error> {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf)?;
    Ok(hex_encode(&buf))
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request;
    use axum::routing::get;
    use tower::ServiceExt;

    fn router_with_auth(token: Option<&str>) -> Router {
        let state = Arc::new(AuthState::new(token.map(str::to_string)));
        Router::new()
            .route("/probe", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
    }

    #[tokio::test]
    async fn no_token_configured_passes_anything_through() {
        let r = router_with_auth(None);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_header_returns_401_with_www_authenticate() {
        let r = router_with_auth(Some("secret"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp.headers().get(header::WWW_AUTHENTICATE).unwrap();
        let www = www.to_str().unwrap();
        // GET 401 advertises BOTH challenges so browsers (Basic) and
        // MCP clients (Bearer) each see what they understand.
        assert!(www.contains("Bearer"));
        assert!(www.contains("Basic"));
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let r = router_with_auth(Some("the-right-one"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-wrong-one")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn right_token_returns_200() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn lowercase_scheme_is_accepted() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "bearer right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_scheme_is_rejected() {
        // `Digest`, `OAuth`, etc. are not handled.
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Digest username=foo,response=bar")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_with_right_token_passes_get() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bearer_header_takes_precedence_over_cookie() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer wrong-token")
                    .header("Cookie", "ai_memory_auth=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_with_wrong_token_fails() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_ignored_on_post() {
        // POST routes must use Bearer header; cookie auth is GET-only
        // to keep the CSRF surface confined to read paths.
        let state = Arc::new(AuthState::new(Some("right-token".to_string())));
        let r = Router::new()
            .route("/probe", axum::routing::post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));
        let resp = r
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Helper: build a Basic-auth header value (any username, token as password).
    fn basic_auth(token: &str) -> String {
        use base64::Engine;
        let creds = format!("any:{token}");
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(creds)
        )
    }

    #[tokio::test]
    async fn basic_auth_with_right_token_passes_get_and_sets_cookie() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", basic_auth("right-token"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // First successful Basic hit also stamps the cookie so the
        // browser doesn't dialog-prompt every session.
        let cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .expect("set-cookie on first Basic-auth success")
            .to_str()
            .unwrap();
        assert!(cookie.contains("ai_memory_auth=right-token"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Path=/"));
    }

    #[tokio::test]
    async fn basic_auth_with_wrong_password_returns_401() {
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", basic_auth("wrong-token"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn basic_auth_ignored_on_post() {
        // POST routes must use Bearer header; Basic auth is GET-only.
        let state = Arc::new(AuthState::new(Some("right-token".to_string())));
        let r = Router::new()
            .route("/probe", axum::routing::post(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));
        let resp = r
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header("Authorization", basic_auth("right-token"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // POST 401 must NOT advertise Basic — browsers would dialog
        // for a route they can't authenticate this way.
        let www = resp.headers().get(header::WWW_AUTHENTICATE).unwrap();
        let www = www.to_str().unwrap();
        assert!(www.contains("Bearer"));
        assert!(!www.contains("Basic"));
    }

    #[tokio::test]
    async fn cookie_request_does_not_re_set_cookie() {
        // Already-authed-by-cookie requests don't need a Set-Cookie
        // refresh; that's a waste of bandwidth on every navigation.
        let r = router_with_auth(Some("right-token"));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Cookie", "ai_memory_auth=right-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
    }

    #[test]
    fn generated_token_is_hex_and_correct_length() {
        let t = generate_token_hex(32).unwrap();
        assert_eq!(t.len(), 64);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // Distinct calls produce distinct tokens (modulo OS RNG bugs).
        let t2 = generate_token_hex(32).unwrap();
        assert_ne!(t, t2);
    }

    // ── Extension<ActorContext> injection (P1.3 multi-rung resolution) ──

    use ai_memory_core::NewUser;
    use ai_memory_store::Store;
    use axum::Extension;
    use tempfile::TempDir;

    /// Route handler that echoes the injected `ActorContext` as JSON
    /// in the response body, so tests can verify which rung fired.
    async fn echo_actor(Extension(actor): Extension<ActorContext>) -> axum::Json<ActorContext> {
        axum::Json(actor)
    }

    async fn echo_auth_level(Extension(level): Extension<AuthLevel>) -> &'static str {
        match level {
            AuthLevel::Anonymous => "anonymous",
            AuthLevel::Root => "root",
            AuthLevel::User => "user",
        }
    }

    fn router_with_state(state: AuthState) -> Router {
        Router::new()
            .route("/probe", get(echo_actor))
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(state),
                require_bearer,
            ))
    }

    async fn body_as_actor(resp: Response) -> ActorContext {
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn rung0_anonymous_attaches_default_actor() {
        // No token configured → middleware is a no-op gate but still
        // injects an anonymous Extension<ActorContext> so handlers
        // always have one to read.
        let r = router_with_state(AuthState::new(None));
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert!(!actor.has_any(), "rung 0 must inject anonymous actor");
    }

    #[tokio::test]
    async fn rung1_root_token_attributes_via_root_actor_template() {
        // Bearer matches config token → middleware stamps the
        // `[auth].root_*` template.
        let root = ActorContext {
            user: Some("boss".into()),
            email: Some("boss@example.com".into()),
            name: Some("Boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into())).with_root_actor(root);
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("boss"));
        assert_eq!(actor.email.as_deref(), Some("boss@example.com"));
        assert_eq!(actor.name.as_deref(), Some("Boss"));
    }

    #[tokio::test]
    async fn rung1_without_root_template_still_authenticates_anonymously() {
        // Backward-compat with existing single-user setups that have
        // bearer_token but no root_username/email/name configured.
        // Bearer matches → 200, but the actor stays anonymous.
        let state = AuthState::new(Some("plain-token".into()));
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer plain-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert!(
            !actor.has_any(),
            "rung-1 sans template must still attribute anonymously"
        );
    }

    /// Fresh store + writer/reader + pre-loaded users row, ready to
    /// plug into [`AuthState::with_multiuser`]. Returns the raw
    /// plaintext token issued to the new user so tests can present it
    /// in the request and assert it routes correctly.
    async fn setup_multiuser(username: &str) -> (TempDir, AuthState, String) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let pepper = TokenPepper::new("test-pepper");
        let token = ai_memory_store::generate_token().unwrap();
        let token_hash = ai_memory_store::hash_token(&token, &pepper);
        let mut new_user = NewUser {
            username: username.into(),
            name: Some(format!("{username} display")),
            email: Some(format!("{username}@example.com")),
        };
        new_user.validate().unwrap();
        store
            .writer
            .create_user(new_user, token_hash)
            .await
            .unwrap();

        let state = AuthState::new(Some("root-token-distinct-from-user-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root".into()),
                ..ActorContext::default()
            })
            .with_multiuser(pepper, store.reader.clone(), store.writer.clone());
        (tmp, state, token)
    }

    #[tokio::test]
    async fn rung2_db_user_token_attributes_to_row() {
        // Bearer doesn't match root, multi-user is enabled, and the
        // token hashes to a `users` row → middleware injects that
        // user's identity (NOT the root template).
        let (_tmp, state, token) = setup_multiuser("alice").await;
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("alice"));
        assert_eq!(actor.email.as_deref(), Some("alice@example.com"));
        assert_eq!(actor.name.as_deref(), Some("alice display"));
        // NOT the root template — root_actor.user is "root".
        assert_ne!(actor.user.as_deref(), Some("root"));
    }

    #[tokio::test]
    async fn rung3_unknown_bearer_with_multiuser_returns_401_not_anonymous() {
        // The bypass guard: bearer present but matches NEITHER root
        // NOR any users row → MUST 401. Critical so a fat-fingered
        // operator (or compromised client) can't squeak through.
        let (_tmp, state, _token) = setup_multiuser("alice").await;
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer this-token-is-not-in-the-DB")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn rung2_expired_user_token_is_rejected() {
        // Expiring a user's token must immediately stop authenticating
        // (no 30s cache window or similar). Critical for `ai-memory
        // user expire` to be useful as an offboarding tool.
        let (_tmp, state, token) = setup_multiuser("alice").await;

        // Look up user id via the writer-side roundtrip (no public
        // find-by-username on WriterHandle, so use the reader pool).
        let user = state
            .multiuser
            .as_ref()
            .unwrap()
            .reader
            .find_user_by_username("alice".into())
            .await
            .unwrap()
            .unwrap();
        let writer = state.multiuser.as_ref().unwrap().writer.clone();
        writer.expire_user_token(user.id).await.unwrap();

        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "expired user token must not authenticate"
        );
    }

    #[tokio::test]
    async fn rung2_revived_user_token_authenticates_again() {
        let (_tmp, state, token) = setup_multiuser("alice").await;
        let user = state
            .multiuser
            .as_ref()
            .unwrap()
            .reader
            .find_user_by_username("alice".into())
            .await
            .unwrap()
            .unwrap();
        let writer = state.multiuser.as_ref().unwrap().writer.clone();
        writer.expire_user_token(user.id).await.unwrap();
        writer.revive_user_token(user.id).await.unwrap();

        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn root_token_wins_even_when_multiuser_is_enabled() {
        // Mixed setup: root token AND added users. Bearer = root token
        // → root_actor template (NOT a users-table lookup that wouldn't
        // find anything anyway).
        let (_tmp, state, _user_token) = setup_multiuser("alice").await;
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header(
                        "Authorization",
                        "Bearer root-token-distinct-from-user-token",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("root"));
    }

    #[tokio::test]
    async fn resolver_authenticates_user_added_after_router_construction() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let pepper = TokenPepper::new("test-pepper");
        let state = AuthState::new(Some("root-token".into())).with_multiuser(
            pepper.clone(),
            store.reader.clone(),
            store.writer.clone(),
        );
        let router = Router::new().route("/level", get(echo_auth_level)).layer(
            axum::middleware::from_fn_with_state(Arc::new(state), require_bearer),
        );

        let token = ai_memory_store::generate_token().unwrap();
        let mut user = NewUser {
            username: "alice".into(),
            name: None,
            email: None,
        };
        user.validate().unwrap();
        store
            .writer
            .create_user(user, ai_memory_store::hash_token(&token, &pepper))
            .await
            .unwrap();

        for (token, expected_status, expected_level) in [
            (token.as_str(), StatusCode::OK, Some("user")),
            ("root-token", StatusCode::OK, Some("root")),
            ("unknown-token", StatusCode::UNAUTHORIZED, None),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/level")
                        .header("Authorization", format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected_status, "token {token}");
            if let Some(expected_level) = expected_level {
                let body = axum::body::to_bytes(response.into_body(), 1024)
                    .await
                    .unwrap();
                assert_eq!(body.as_ref(), expected_level.as_bytes());
            }
        }
    }

    #[test]
    fn blank_root_token_is_not_enabled() {
        assert!(!AuthState::new(Some("  ".into())).enabled());
    }

    #[tokio::test]
    async fn rung1_setup_with_unknown_bearer_returns_401() {
        // Existing single-user setup (rung 1 only, no multi-user). An
        // unknown bearer must still 401 — same as pre-P1.3 behaviour.
        let state = AuthState::new(Some("right-token".into())).with_root_actor(ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        });
        let r = router_with_state(state);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
