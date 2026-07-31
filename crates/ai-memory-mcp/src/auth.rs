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
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
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
    /// Shared secret that a trusted authenticating proxy presents to assert an
    /// end-user identity via `X-Memory-Actor-*` headers — see
    /// [`Self::with_trusted_proxy`]. `None` (the default) means those headers
    /// are ignored entirely, which is the only safe default for a server whose
    /// port anything on the network can reach.
    actor_proxy_secret: Option<String>,
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

    /// Does `actor` name the operator configured as root?
    ///
    /// Used to decide whether a proxy-asserted identity keeps root capability.
    /// With no `root_username` configured there is no one to match, so nobody
    /// the proxy names is root.
    fn asserts_root_identity(&self, actor: &ActorContext) -> bool {
        match (self.root_actor.user.as_deref(), actor.user.as_deref()) {
            (Some(root), Some(asserted)) => root == asserted,
            _ => false,
        }
    }

    /// Trust an authenticating proxy to assert WHO the caller is.
    ///
    /// A proxy that terminates SSO (validating an OIDC token, say) usually
    /// cannot forward the end user's credential upstream: it authenticates to
    /// this server with the single root token and describes the human in
    /// `X-Memory-Actor-*` headers. Without a way to tell that proxy apart from
    /// any other client, those headers cannot be believed — anyone able to
    /// reach the port could claim to be anyone — so they are ignored by
    /// default and every caller collapses into the one root identity.
    ///
    /// Presenting `secret` in `X-Memory-Actor-Proxy-Secret` on a request that
    /// already authenticated as root lets the headers overlay the root actor.
    /// The secret IS the switch: there is no separate "trust headers" flag to
    /// turn on without one, so this cannot be enabled insecurely by accident.
    /// A blank secret is treated as absent.
    #[must_use]
    pub fn with_trusted_proxy(mut self, secret: impl Into<String>) -> Self {
        let secret = secret.into();
        self.actor_proxy_secret = Some(secret).filter(|s| !s.trim().is_empty());
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
/// Header a trusted proxy uses to prove it is the proxy.
const ACTOR_PROXY_SECRET_HEADER: &str = "x-memory-actor-proxy-secret";

/// Every header the trusted-proxy overlay reads, including the secret itself.
/// A repeated occurrence of any of them makes the assertion ambiguous — see
/// [`overlay_trusted_proxy_actor`].
const PROXY_ASSERTION_HEADERS: [&str; 6] = [
    ACTOR_PROXY_SECRET_HEADER,
    "x-memory-actor-user",
    "x-memory-actor-sub",
    "x-memory-actor-agent",
    "x-memory-actor-client",
    "x-memory-actor-session-id",
];

/// What the trusted-proxy overlay concluded about a request.
enum ProxyAssertion {
    /// No overlay: no configured secret, no secret header, or a mismatch. The
    /// root actor is untouched and the caller stays plain root.
    Absent,
    /// The proxy proved itself; the actor now carries whatever it asserted,
    /// which may be nobody.
    Applied,
    /// The proxy's headers arrived more than once, so WHO the caller is cannot
    /// be decided. The request must fail rather than pick a value.
    Ambiguous,
}

/// Let a trusted proxy say WHO the caller is, overlaying the asserted identity
/// onto the root actor.
///
/// Everything is fail-closed: no configured secret, a missing header, or a
/// mismatch all leave `actor` untouched, so the pre-existing behaviour (every
/// proxied caller collapses into the single root identity) is what you get
/// until an operator deliberately configures the shared secret.
///
/// Only fields the proxy actually asserts are overlaid; the root actor's own
/// values survive for anything the proxy left out.
///
/// # The proxy MUST strip client-supplied `X-Memory-Actor-*` headers
///
/// This whole overlay assumes the `X-Memory-Actor-*` values on the wire are the
/// proxy's, not the caller's. An ingress configured to *append* its headers
/// rather than replace them leaves the client's value in place beside the
/// proxy's, and there is no way to tell which is which — so a duplicate is
/// treated as a spoofing attempt and the request is refused
/// ([`ProxyAssertion::Ambiguous`]) instead of one of the two being picked.
fn overlay_trusted_proxy_actor(
    state: &AuthState,
    headers: &HeaderMap,
    actor: &mut ActorContext,
) -> ProxyAssertion {
    let Some(expected) = state.actor_proxy_secret.as_deref() else {
        return ProxyAssertion::Absent;
    };
    // Checked before the comparison below because `HeaderMap::get` returns the
    // FIRST value: a client that prepends its own secret header would otherwise
    // force a mismatch, suppress the proxy's assertion, and be handed the
    // undowngraded root identity — an escalation, not just a lost overlay.
    if PROXY_ASSERTION_HEADERS
        .iter()
        .any(|name| headers.get_all(*name).iter().count() > 1)
    {
        return ProxyAssertion::Ambiguous;
    }
    let presented = headers
        .get(ACTOR_PROXY_SECRET_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    // Constant-time: this is a credential comparison like the bearer above.
    // Differing lengths already compare unequal, so an absent header is safe.
    if !bool::from(presented.as_bytes().ct_eq(expected.as_bytes())) {
        return ProxyAssertion::Absent;
    }
    let asserted = crate::actor::actor_from_headers(headers);
    // The proxy owns the IDENTITY fields wholesale: user, name, email and sub
    // are taken from what it asserts, never merged with the root template.
    // Merging per-field looks harmless but is not — a proxy that forwards only
    // `sub` would leave the root template's `user` in place, so every human
    // would share one owner while carrying someone else's subject. Taking the
    // block as a unit means a proxy that names nobody yields an unattributed
    // actor (shared-only access), which is the fail-safe direction.
    actor.user = asserted.user;
    actor.sub = asserted.sub;
    actor.name = None;
    actor.email = None;
    // Transport/agent detail is not identity: keep whatever the request layer
    // already knew when the proxy does not describe it.
    if asserted.client.is_some() {
        actor.client = asserted.client;
    }
    if asserted.agent.is_some() {
        actor.agent = asserted.agent;
    }
    if asserted.session_id.is_some() {
        actor.session_id = asserted.session_id;
    }
    ProxyAssertion::Applied
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
        // Rung 1b: a trusted proxy may name the real human behind this root
        // credential. No-op unless the shared secret is configured AND
        // presented, so an untrusted client cannot assert an identity by
        // setting headers.
        let assertion = overlay_trusted_proxy_actor(&state, req.headers(), &mut actor);
        if matches!(assertion, ProxyAssertion::Ambiguous) {
            debug!("auth rejected: duplicated trusted-proxy identity header");
            return ambiguous_proxy_identity();
        }
        // The credential is root's, but the human it stands for usually is not.
        // Keeping AuthLevel::Root here would hand root capability to every
        // person the proxy authenticates, which would silently void the
        // admin-gated operations (sweep, purge, delete) for exactly the
        // multi-operator deployment this overlay exists to serve. Only a
        // caller the proxy names AS the configured root operator stays root.
        //
        // The downgrade needs an asserted IDENTITY, not merely a valid secret:
        // a proxy that echoes the secret on its own health checks or on
        // machine-to-machine traffic asserts nobody, and demoting those would
        // strip root from the deployment's own maintenance calls.
        //
        // "Somebody" cannot mean `user` alone. An ingress that terminates OIDC
        // and forwards only the subject claim names a human just as much — one
        // that can never match `root_username` — so keying on `user` would hand
        // root to every person behind exactly the proxy this overlay serves.
        // Any identity field the overlay adopts counts.
        let named_someone = matches!(assertion, ProxyAssertion::Applied)
            && (actor.user.is_some() || actor.sub.is_some());
        let level = if named_someone && !state.asserts_root_identity(&actor) {
            AuthLevel::User
        } else {
            AuthLevel::Root
        };
        if matches!(assertion, ProxyAssertion::Applied) {
            debug!(actor.user = ?actor.user, ?level, "identity asserted by trusted proxy");
        }
        req.extensions_mut().insert(actor);
        req.extensions_mut().insert(level);

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

/// The proxy's identity headers contradict themselves. Not a 401 — the
/// credential was fine; the request itself is malformed, and retrying it with
/// the same headers will keep failing until the ingress is fixed to REPLACE
/// `X-Memory-Actor-*` rather than append to whatever the client sent.
fn ambiguous_proxy_identity() -> Response {
    (
        StatusCode::BAD_REQUEST,
        "duplicate X-Memory-Actor-* header: the proxy must replace \
         client-supplied actor headers, not append to them\n",
    )
        .into_response()
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

    /// A proxy-asserted human must NOT inherit root capability: the credential
    /// belongs to the proxy, the identity belongs to an ordinary person, and
    /// admin-gated operations (sweep, purge, delete) key off the level.
    #[tokio::test]
    async fn proxy_asserted_identity_is_downgraded_to_user_level() {
        let root = ActorContext {
            user: Some("root-operator".into()),
            ..ActorContext::default()
        };
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(root)
                .with_trusted_proxy("proxy-shared-secret"),
        );
        let router = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));

        let level_for = |user: &'static str| {
            let router = router.clone();
            async move {
                let resp = router
                    .oneshot(
                        Request::builder()
                            .uri("/level")
                            .header("Authorization", "Bearer the-root-token")
                            .header("X-Memory-Actor-Proxy-Secret", "proxy-shared-secret")
                            .header("X-Memory-Actor-User", user)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                String::from_utf8(bytes.to_vec()).unwrap()
            }
        };

        assert_eq!(
            level_for("alice").await,
            "user",
            "an ordinary human behind the proxy must not hold root capability"
        );
        // The operator the server is configured to treat as root keeps it.
        assert_eq!(level_for("root-operator").await, "root");
    }

    /// The proxy's own traffic — health checks, machine-to-machine calls —
    /// carries the secret but names nobody. Downgrading on the secret alone
    /// would strip root from those for no reason: there is no other human whose
    /// authority the request could be standing in for.
    #[tokio::test]
    async fn proxy_secret_without_an_asserted_user_keeps_root_level() {
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(ActorContext {
                    user: Some("root-operator".into()),
                    ..ActorContext::default()
                })
                .with_trusted_proxy("proxy-shared-secret"),
        );
        let resp = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
            .oneshot(
                Request::builder()
                    .uri("/level")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-Proxy-Secret", "proxy-shared-secret")
                    .header("X-Memory-Actor-Agent", "healthcheck")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), "root");
    }

    /// An ingress that terminates OIDC and forwards only the subject claim
    /// (no `preferred_username`, so no `X-Memory-Actor-User`) still names a
    /// human. That human can never match `root_username`, so keeping root here
    /// would hand purge, delete and the forget sweep to everyone behind the
    /// proxy — worse than the collapse-into-root this overlay replaces.
    #[tokio::test]
    async fn proxy_asserting_only_sub_is_downgraded_to_user_level() {
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(ActorContext {
                    user: Some("root-operator".into()),
                    ..ActorContext::default()
                })
                .with_trusted_proxy("proxy-shared-secret"),
        );
        let resp = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
            .oneshot(
                Request::builder()
                    .uri("/level")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-Proxy-Secret", "proxy-shared-secret")
                    .header("X-Memory-Actor-Sub", "oidc-subject-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(bytes.to_vec()).unwrap(),
            "user",
            "a sub-only assertion names somebody who is not the root operator"
        );
    }

    /// An ingress that APPENDS `X-Memory-Actor-User` instead of replacing it
    /// leaves the caller's own value first in the list, and `HeaderMap::get`
    /// returns the first — so Bob sending `X-Memory-Actor-User: alice` would be
    /// Alice everywhere. Neither value may be adopted: refuse the request.
    #[tokio::test]
    async fn duplicated_actor_user_header_is_refused() {
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root-operator".into()),
                ..ActorContext::default()
            })
            .with_trusted_proxy("proxy-shared-secret");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-Proxy-Secret", "proxy-shared-secret")
                    // Bob's own header, then the proxy's appended one.
                    .header("X-Memory-Actor-User", "alice")
                    .header("X-Memory-Actor-User", "bob")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "an ambiguous asserted identity must fail closed, not resolve to one of the two"
        );
    }

    /// The secret header has the same first-wins hazard, and worse consequences:
    /// a client that prepends garbage forces a mismatch, suppresses the proxy's
    /// assertion, and would be handed the undowngraded ROOT identity.
    #[tokio::test]
    async fn duplicated_proxy_secret_header_is_refused() {
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root-operator".into()),
                ..ActorContext::default()
            })
            .with_trusted_proxy("proxy-shared-secret");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-Proxy-Secret", "not-the-secret")
                    .header("X-Memory-Actor-Proxy-Secret", "proxy-shared-secret")
                    .header("X-Memory-Actor-User", "bob")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Without a proxy assertion the root bearer keeps root, unchanged.
    #[tokio::test]
    async fn plain_root_bearer_keeps_root_level() {
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(ActorContext {
                    user: Some("root-operator".into()),
                    ..ActorContext::default()
                })
                .with_trusted_proxy("proxy-shared-secret"),
        );
        let resp = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
            .oneshot(
                Request::builder()
                    .uri("/level")
                    .header("Authorization", "Bearer the-root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(bytes.to_vec()).unwrap(), "root");
    }

    /// A proxy that forwards only the subject names nobody, so the actor must
    /// come out unattributed rather than silently keeping the root username —
    /// otherwise every human collapses onto one owner carrying a foreign `sub`.
    #[tokio::test]
    async fn proxy_asserting_only_sub_yields_an_unattributed_user() {
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root-operator".into()),
                email: Some("root@example.com".into()),
                ..ActorContext::default()
            })
            .with_trusted_proxy("proxy-shared-secret");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-Proxy-Secret", "proxy-shared-secret")
                    .header("X-Memory-Actor-Sub", "oidc-subject-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user, None, "the root username must not survive");
        assert_eq!(actor.sub.as_deref(), Some("oidc-subject-123"));
        assert_eq!(actor.email, None);
    }

    /// Security boundary: the `X-Memory-Actor-*` headers are pure client input.
    /// With no proxy secret configured they must be ignored completely, or
    /// anyone who can reach the port authenticates as root and then names
    /// themselves whoever they like.
    #[tokio::test]
    async fn actor_headers_are_ignored_without_a_configured_proxy_secret() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into())).with_root_actor(root);
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-User", "impostor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let actor = body_as_actor(resp).await;
        assert_eq!(
            actor.user.as_deref(),
            Some("boss"),
            "unproven actor headers must not override the root identity"
        );
    }

    /// Same headers, secret configured but NOT presented: still ignored.
    #[tokio::test]
    async fn actor_headers_are_ignored_without_the_proxy_secret_header() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(root)
            .with_trusted_proxy("proxy-shared-secret");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-User", "impostor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("boss"));
    }

    /// A wrong secret is as good as none.
    #[tokio::test]
    async fn actor_headers_are_ignored_when_the_proxy_secret_mismatches() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(root)
            .with_trusted_proxy("proxy-shared-secret");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-Proxy-Secret", "wrong")
                    .header("X-Memory-Actor-User", "impostor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("boss"));
    }

    /// The point of the feature: with the secret proven, two different humans
    /// behind the same root credential become two different actors.
    #[tokio::test]
    async fn trusted_proxy_identity_overlays_the_root_actor() {
        let root = ActorContext {
            user: Some("boss".into()),
            email: Some("boss@example.com".into()),
            name: Some("Boss".into()),
            ..ActorContext::default()
        };
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(root)
                .with_trusted_proxy("proxy-shared-secret"),
        );
        let router = Router::new()
            .route("/probe", get(echo_actor))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));

        for user in ["alice", "bob"] {
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/probe")
                        .header("Authorization", "Bearer the-root-token")
                        .header("X-Memory-Actor-Proxy-Secret", "proxy-shared-secret")
                        .header("X-Memory-Actor-User", user)
                        .header("X-Memory-Actor-Session-Id", format!("sess-{user}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let actor = body_as_actor(resp).await;
            assert_eq!(actor.user.as_deref(), Some(user));
            assert_eq!(actor.session_id.as_deref(), Some(&*format!("sess-{user}")));
            // The root template's contact details describe the root operator,
            // not the human just named, so they must not be carried over.
            assert_eq!(actor.email, None);
            assert_eq!(actor.name, None);
        }
    }

    /// A blank secret must not enable the overlay — otherwise an empty config
    /// value would make a missing header compare equal and trust everyone.
    #[tokio::test]
    async fn blank_proxy_secret_does_not_enable_the_overlay() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(root)
            .with_trusted_proxy("   ");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer the-root-token")
                    .header("X-Memory-Actor-User", "impostor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user.as_deref(), Some("boss"));
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
