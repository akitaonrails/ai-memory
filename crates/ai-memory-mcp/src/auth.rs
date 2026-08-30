//! Authorization middleware for the HTTP server.
//!
//! Machine-only routes (`/mcp`, `/hook`, `/handoff`,
//! `/workstream/*`) accept `Authorization: Bearer` only. Browser
//! routes use Bearer plus one of two mutually exclusive modes:
//!
//! - before human password auth is configured, GET requests retain
//!   the deprecated Basic / `ai_memory_auth` cookie transport;
//! - after human auth is configured, only short-lived human sessions
//!   authenticate browser requests.
//!
//! When no authority is configured the middleware is a no-op,
//! preserving zero-config loopback.
//!
//! Comparison uses [`subtle::ConstantTimeEq`] so an attacker on the
//! same LAN cannot use response-time leaks to recover the token byte
//! by byte.

use std::sync::Arc;

use ai_memory_core::{ActorContext, AuthLevel, IdentityKey};
use ai_memory_store::{ReaderPool, TokenPepper, WriterHandle, hash_token};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;
use tracing::debug;

/// Realm advertised in `WWW-Authenticate` Bearer challenges.
const AUTH_REALM: &str = "ai-memory";

/// Outcome of inspecting `Authorization: Bearer` without running the
/// rest of the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BearerAuth {
    /// Actor and [`AuthLevel`] were injected into the request.
    Authenticated,
    /// No Bearer scheme. Basic, empty, unknown, or missing Authorization
    /// do not count as a presented Bearer.
    Absent,
    /// Bearer scheme was present and did not authenticate.
    Rejected,
}

/// Optional multi-user resolver tier — token-hash lookup against the
/// `api_credentials` table. Populated only when both a per-server pepper and a
/// reader pool are available (i.e. after `ai-memory init` ran and a
/// store was opened). Single-user (rung-1) setups skip this entirely;
/// rung-0 (no auth) skips the whole middleware.
#[derive(Clone)]
pub struct MultiUserResolver {
    /// Hashes incoming tokens with the per-server pepper before the
    /// `api_credentials.token_hash` lookup.
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
    /// Whether browser session cookies must be marked `Secure`. This is an
    /// explicit operator setting: proxy headers are untrusted input.
    secure_cookie: bool,
    /// Actor template stamped onto requests that authenticate with the
    /// `expected` token. Populated from `[auth].root_username` /
    /// `root_email` / `root_name`. When all three are unset the root
    /// actor stays anonymous (rung-1 backward-compat: bearer
    /// authenticates but the audit log records nothing identifying).
    root_actor: ActorContext,
    /// Multi-user lookup tier. `None` until both pepper and reader
    /// are available — see [`Self::with_multiuser`].
    multiuser: Option<MultiUserResolver>,
    /// Dedicated bearer credential for a trusted authenticating proxy. It is
    /// separate from the root bearer so a missing identity assertion cannot
    /// accidentally turn proxy traffic into root traffic.
    actor_proxy_bearer: Option<String>,
    /// Human password/session runtime. `None` for machine-only or
    /// zero-config anonymous loopback.
    pub human: Option<crate::human_auth::HumanAuthRuntime>,
    /// Whether human auth was intended at startup. Runtime presence is
    /// separate because the browser transition checks persisted password /
    /// bootstrap state on every request.
    human_intended: bool,
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

    /// Require HTTPS for browser session cookies.
    #[must_use]
    pub fn with_secure_cookie(mut self, secure_cookie: bool) -> Self {
        self.secure_cookie = secure_cookie;
        self
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
    /// Only the stable OIDC pair can match; usernames are display data and
    /// never grant root.
    fn asserts_root_identity(&self, actor: &ActorContext) -> bool {
        let Some(root @ IdentityKey::Subject { .. }) = self.root_actor.identity_key() else {
            return false;
        };
        actor
            .identity_key()
            .is_some_and(|asserted| root == asserted)
    }

    /// Trust an authenticating proxy to assert WHO the caller is.
    ///
    /// A proxy that terminates SSO (validating an OIDC token, say) usually
    /// cannot forward the end user's credential upstream: it authenticates to
    /// this server with a dedicated proxy bearer and describes the human in
    /// `X-Memory-Actor-*` headers. Without a way to tell that proxy apart from
    /// any other client, those headers cannot be believed — anyone able to
    /// reach the port could claim to be anyone — so they are ignored by
    /// default.
    ///
    /// Presenting this bearer authenticates only the proxy rung. A blank value
    /// is treated as absent.
    #[must_use]
    pub fn with_trusted_proxy_bearer(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        self.actor_proxy_bearer = Some(token).filter(|s| !s.trim().is_empty());
        self
    }

    /// Enable multi-user lookups: a bearer that doesn't match the root
    /// token is hashed with `pepper` and checked against
    /// `api_credentials`; a hit attributes the request to that user.
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

    /// Attach an active human password/session runtime.
    #[must_use]
    pub fn with_human(self, human: crate::human_auth::HumanAuthRuntime) -> Self {
        self.with_human_runtime(human, true)
    }

    /// Attach the runtime while preserving the startup decision about whether
    /// human auth contributes authority. HTTP serving attaches the runtime
    /// even before bootstrap so the transition can happen without restart.
    #[must_use]
    pub fn with_human_runtime(
        mut self,
        human: crate::human_auth::HumanAuthRuntime,
        human_intended: bool,
    ) -> Self {
        self.human = Some(human);
        self.human_intended = human_intended;
        self
    }

    /// Per-server token pepper when multi-user lookup is enabled.
    #[must_use]
    pub fn pepper(&self) -> Option<&TokenPepper> {
        self.multiuser.as_ref().map(|mu| &mu.pepper)
    }

    /// Whether browser cookies must be marked `Secure`.
    #[must_use]
    pub fn secure_cookie(&self) -> bool {
        self.secure_cookie
    }

    /// Dedicated actor-proxy bearer, if configured.
    #[must_use]
    pub fn actor_proxy_bearer(&self) -> Option<&str> {
        self.actor_proxy_bearer.as_deref()
    }

    /// True when machine authority or startup-intended human auth is
    /// configured.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.expected.is_some() || self.human_intended
    }
}

/// Every header the trusted-proxy assertion reads.
/// A repeated occurrence of any of them makes the assertion ambiguous — see
/// [`trusted_proxy_actor`].
const PROXY_ASSERTION_HEADERS: [&str; 6] = [
    "x-memory-actor-user",
    "x-memory-actor-issuer",
    "x-memory-actor-sub",
    "x-memory-actor-agent",
    "x-memory-actor-client",
    "x-memory-actor-session-id",
];

/// Why a trusted-proxy identity assertion was rejected.
#[derive(Debug)]
enum ProxyAssertionError {
    /// The proxy's headers arrived more than once, so WHO the caller is cannot
    /// be decided. The request must fail rather than pick a value.
    Ambiguous,
    /// A proxy credential must always name a human.
    MissingIdentity,
    /// OIDC issuer and subject are accepted only together.
    PartialOidcIdentity,
}

/// Parse the identity asserted by an already-authenticated proxy.
///
/// # The proxy MUST strip client-supplied `X-Memory-Actor-*` headers
///
/// This whole overlay assumes the `X-Memory-Actor-*` values on the wire are the
/// proxy's, not the caller's. An ingress configured to *append* its headers
/// rather than replace them leaves the client's value in place beside the
/// proxy's, and there is no way to tell which is which — so a duplicate is
/// treated as a spoofing attempt and the request is refused
/// ([`ProxyAssertionError::Ambiguous`]) instead of one of the two being picked.
fn trusted_proxy_actor(headers: &HeaderMap) -> Result<ActorContext, ProxyAssertionError> {
    if PROXY_ASSERTION_HEADERS.iter().any(|name| {
        headers.get_all(*name).iter().count() > 1
            || headers
                .get(*name)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.contains(','))
    }) {
        return Err(ProxyAssertionError::Ambiguous);
    }
    let asserted = crate::actor::actor_from_headers(headers);
    if asserted.issuer.is_some() != asserted.sub.is_some() {
        return Err(ProxyAssertionError::PartialOidcIdentity);
    }
    if asserted.identity_key().is_none() {
        return Err(ProxyAssertionError::MissingIdentity);
    }
    Ok(asserted)
}

/// Inspect `Authorization: Bearer` and inject actor / [`AuthLevel`] on
/// success.
///
/// * [`BearerAuth::Authenticated`] — extensions populated; caller runs `next`.
/// * [`BearerAuth::Absent`] — no Bearer scheme (missing, Basic, empty, unknown).
/// * [`BearerAuth::Rejected`] — Bearer scheme was present and did not authenticate.
/// * `Err(Response)` — trusted-proxy assertion is malformed (400).
pub async fn authenticate_bearer(
    state: &AuthState,
    req: &mut Request<axum::body::Body>,
) -> Result<BearerAuth, Response> {
    let Some(provided) = extract_bearer_from_headers(req.headers()) else {
        return Ok(BearerAuth::Absent);
    };
    authenticate_token(state, req, &provided, true).await
}

/// Authenticate one already-extracted token.
///
/// Browser Basic/cookie compatibility calls this with `allow_proxy=false`:
/// proxy credentials may assert identities only through an explicit Bearer.
pub(crate) async fn authenticate_token(
    state: &AuthState,
    req: &mut Request<axum::body::Body>,
    provided: &str,
    allow_proxy: bool,
) -> Result<BearerAuth, Response> {
    if provided.is_empty() {
        return Ok(BearerAuth::Rejected);
    }

    if let Some(expected) = state.expected.as_deref()
        && bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
    {
        req.extensions_mut().insert(state.root_actor.clone());
        req.extensions_mut().insert(AuthLevel::Root);
        return Ok(BearerAuth::Authenticated);
    }

    if allow_proxy
        && let Some(proxy_expected) = state.actor_proxy_bearer.as_deref()
        && bool::from(provided.as_bytes().ct_eq(proxy_expected.as_bytes()))
    {
        let actor = match trusted_proxy_actor(req.headers()) {
            Ok(actor) => actor,
            Err(error) => {
                debug!(
                    ?error,
                    "auth rejected: invalid trusted-proxy identity assertion"
                );
                return Err(invalid_proxy_identity(error));
            }
        };
        let level = if state.asserts_root_identity(&actor) {
            AuthLevel::Root
        } else {
            AuthLevel::User
        };
        debug!(
            actor.user = ?actor.user,
            actor.issuer = ?actor.issuer,
            actor.sub = ?actor.sub,
            ?level,
            "identity asserted by trusted proxy"
        );
        req.extensions_mut().insert(actor);
        req.extensions_mut().insert(level);
        return Ok(BearerAuth::Authenticated);
    }

    if let Some(mu) = state.multiuser.as_ref() {
        let hash = hash_token(provided, &mu.pepper);
        match mu.reader.find_active_user_by_token_hash(hash).await {
            Ok(Some(hit)) => {
                debug!(actor.user = %hit.user.username, "authenticated as DB user");
                let actor = ActorContext {
                    user: Some(hit.user.username.clone()),
                    name: hit.user.name.clone(),
                    email: hit.user.email.clone(),
                    ..ActorContext::default()
                };
                req.extensions_mut().insert(actor);
                req.extensions_mut().insert(hit.user.id);
                req.extensions_mut().insert(AuthLevel::User);
                let writer = mu.writer.clone();
                let user_id = hit.user.id;
                let credential_id = hit.credential_id;
                tokio::spawn(async move {
                    if let Err(e) = writer.touch_api_credential(credential_id).await {
                        tracing::warn!(
                            error = %e,
                            credential_id = %credential_id,
                            "touch_api_credential failed"
                        );
                    }
                    if let Err(e) = writer.touch_user_last_seen(user_id).await {
                        tracing::warn!(error = %e, user_id = %user_id, "touch_user_last_seen failed");
                    }
                });
                return Ok(BearerAuth::Authenticated);
            }
            Ok(None) => return Ok(BearerAuth::Rejected),
            Err(e) => {
                tracing::error!(error = %e, "auth: api_credentials lookup failed");
                return Ok(BearerAuth::Rejected);
            }
        }
    }

    Ok(BearerAuth::Rejected)
}

/// Machine-only middleware. Wire with
/// `axum::middleware::from_fn_with_state(state, require_bearer)`.
///
/// Anonymous passthrough happens only when [`AuthState::enabled`] is
/// false (zero-config loopback). Human mode without a presented Bearer
/// is 401 — sessions never authenticate `/mcp`.
pub async fn require_bearer(
    State(state): State<Arc<AuthState>>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    match authenticate_bearer(&state, &mut req).await {
        Ok(BearerAuth::Authenticated) => next.run(req).await,
        Err(resp) => resp,
        Ok(BearerAuth::Absent) if !state.enabled() => {
            req.extensions_mut().insert(ActorContext::anonymous());
            req.extensions_mut().insert(AuthLevel::Anonymous);
            next.run(req).await
        }
        Ok(BearerAuth::Absent | BearerAuth::Rejected) => {
            debug!("auth rejected: invalid or missing token");
            unauthorized_bearer()
        }
    }
}

/// Bearer token value from all `Authorization` occurrences.
///
/// Basic, unknown, and empty headers are discarded. Exactly one Bearer
/// attempt is accepted; repeated Bearer attempts collapse to an empty
/// value so authentication fails closed rather than choosing one.
#[must_use]
pub fn extract_bearer_from_headers(headers: &HeaderMap) -> Option<String> {
    let mut bearer = None;
    for value in headers.get_all(header::AUTHORIZATION) {
        let Ok(raw) = value.to_str() else {
            continue;
        };
        let trimmed = raw.trim();
        let split_at = trimmed.find([' ', '\t']);
        let (scheme, token) = split_at.map_or((trimmed, ""), |idx| {
            (&trimmed[..idx], trimmed[idx..].trim())
        });
        if !scheme.eq_ignore_ascii_case("Bearer") {
            continue;
        }
        if bearer.is_some() {
            return Some(String::new());
        }
        bearer = Some(token.to_string());
    }
    bearer
}

/// True when `Authorization` uses the Bearer scheme, even with no token.
#[must_use]
pub fn has_bearer_scheme(headers: &HeaderMap) -> bool {
    extract_bearer_from_headers(headers).is_some()
}

/// True when any trusted-proxy identity header is present.
#[must_use]
pub fn any_actor_header(headers: &HeaderMap) -> bool {
    PROXY_ASSERTION_HEADERS
        .iter()
        .any(|name| headers.contains_key(*name))
}

/// 401 with `WWW-Authenticate: Bearer` only. Never advertise Basic.
#[must_use]
pub fn unauthorized_bearer() -> Response {
    let mut resp = (StatusCode::UNAUTHORIZED, "auth required\n").into_response();
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        format!("Bearer realm=\"{AUTH_REALM}\", error=\"invalid_token\"")
            .parse()
            .expect("static header value is valid"),
    );
    resp
}

/// The proxy's identity headers contradict themselves. Not a 401 — the
/// credential was fine; the request itself is malformed, and retrying it with
/// the same headers will keep failing until the ingress is fixed to REPLACE
/// `X-Memory-Actor-*` rather than append to whatever the client sent.
fn invalid_proxy_identity(error: ProxyAssertionError) -> Response {
    let message = match error {
        ProxyAssertionError::Ambiguous => {
            "ambiguous X-Memory-Actor-* header: the proxy must replace client-supplied actor headers, not append to them\n"
        }
        ProxyAssertionError::MissingIdentity => {
            "trusted proxy must assert X-Memory-Actor-User or both X-Memory-Actor-Issuer and X-Memory-Actor-Sub\n"
        }
        ProxyAssertionError::PartialOidcIdentity => {
            "trusted proxy must assert X-Memory-Actor-Issuer and X-Memory-Actor-Sub together\n"
        }
    };
    (StatusCode::BAD_REQUEST, message).into_response()
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
        router_with_auth_secure_cookie(token, false)
    }

    fn router_with_auth_secure_cookie(token: Option<&str>, secure_cookie: bool) -> Router {
        let state =
            Arc::new(AuthState::new(token.map(str::to_string)).with_secure_cookie(secure_cookie));
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
        assert!(www.contains("Bearer"));
        assert!(!www.contains("Basic"));
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
    async fn cookie_is_never_a_machine_credential() {
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
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
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
    async fn basic_auth_is_never_a_machine_credential() {
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
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
        let www = resp.headers().get(header::WWW_AUTHENTICATE).unwrap();
        let www = www.to_str().unwrap();
        assert!(www.contains("Bearer"));
        assert!(!www.contains("Basic"));
    }

    #[tokio::test]
    async fn basic_auth_ignored_on_post() {
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
        let www = resp.headers().get(header::WWW_AUTHENTICATE).unwrap();
        let www = www.to_str().unwrap();
        assert!(www.contains("Bearer"));
        assert!(!www.contains("Basic"));
    }

    #[test]
    fn bearer_parser_discards_other_schemes_and_accepts_tabs() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Basic saved-browser-value"),
        );
        headers.append(
            header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("bEaReR\tmachine-token"),
        );
        assert_eq!(
            extract_bearer_from_headers(&headers).as_deref(),
            Some("machine-token")
        );
    }

    #[test]
    fn bearer_parser_rejects_multiple_bearer_attempts() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer first"),
        );
        headers.append(
            header::AUTHORIZATION,
            axum::http::HeaderValue::from_static("Bearer second"),
        );
        assert_eq!(extract_bearer_from_headers(&headers).as_deref(), Some(""));
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
                .with_trusted_proxy_bearer("proxy-bearer-token"),
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
                            .header("Authorization", "Bearer proxy-bearer-token")
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

        for user in ["alice", "root-operator"] {
            assert_eq!(
                level_for(user).await,
                "user",
                "a proxy username alone must never grant root capability"
            );
        }
    }

    /// A proxy credential without a human assertion must fail closed.
    #[tokio::test]
    async fn proxy_bearer_without_an_asserted_identity_is_refused() {
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(ActorContext {
                    user: Some("root-operator".into()),
                    ..ActorContext::default()
                })
                .with_trusted_proxy_bearer("proxy-bearer-token"),
        );
        let resp = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
            .oneshot(
                Request::builder()
                    .uri("/level")
                    .header("Authorization", "Bearer proxy-bearer-token")
                    .header("X-Memory-Actor-Agent", "healthcheck")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// An ingress that terminates OIDC can name a human with the stable pair
    /// even when it does not forward `preferred_username`.
    #[tokio::test]
    async fn proxy_asserting_oidc_identity_is_downgraded_to_user_level() {
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(ActorContext {
                    user: Some("root-operator".into()),
                    ..ActorContext::default()
                })
                .with_trusted_proxy_bearer("proxy-bearer-token"),
        );
        let resp = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer))
            .oneshot(
                Request::builder()
                    .uri("/level")
                    .header("Authorization", "Bearer proxy-bearer-token")
                    .header("X-Memory-Actor-Issuer", "https://idp.example")
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
            "an OIDC assertion names somebody who is not the root operator"
        );
    }

    #[tokio::test]
    async fn oidc_root_requires_the_exact_issuer_and_subject_pair() {
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(ActorContext {
                    user: Some("root-operator".into()),
                    issuer: Some("https://idp.example".into()),
                    sub: Some("root-subject".into()),
                    ..ActorContext::default()
                })
                .with_trusted_proxy_bearer("proxy-bearer-token"),
        );
        let router = Router::new()
            .route("/level", get(echo_auth_level))
            .layer(axum::middleware::from_fn_with_state(state, require_bearer));
        let level_for = |issuer: &'static str, subject: &'static str| {
            let router = router.clone();
            async move {
                let response = router
                    .oneshot(
                        Request::builder()
                            .uri("/level")
                            .header("Authorization", "Bearer proxy-bearer-token")
                            .header("X-Memory-Actor-User", "root-operator")
                            .header("X-Memory-Actor-Issuer", issuer)
                            .header("X-Memory-Actor-Sub", subject)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                    .await
                    .unwrap();
                String::from_utf8(bytes.to_vec()).unwrap()
            }
        };

        assert_eq!(
            level_for("https://idp.example", "root-subject").await,
            "root"
        );
        assert_eq!(
            level_for("https://other-idp.example", "root-subject").await,
            "user"
        );
        assert_eq!(
            level_for("https://idp.example", "someone-else").await,
            "user"
        );
    }

    #[tokio::test]
    async fn partial_oidc_identity_is_refused() {
        let state = AuthState::new(Some("the-root-token".into()))
            .with_trusted_proxy_bearer("proxy-bearer-token");
        for (name, value) in [
            ("X-Memory-Actor-Issuer", "https://idp.example"),
            ("X-Memory-Actor-Sub", "subject-only"),
        ] {
            let response = router_with_state(state.clone())
                .oneshot(
                    Request::builder()
                        .uri("/probe")
                        .header("Authorization", "Bearer proxy-bearer-token")
                        .header(name, value)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "header {name}");
        }
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
            .with_trusted_proxy_bearer("proxy-bearer-token");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer proxy-bearer-token")
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

    /// Some proxies fold repeated headers into a comma-separated value. Reject
    /// that representation too rather than choosing one asserted identity.
    #[tokio::test]
    async fn folded_actor_user_header_is_refused() {
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root-operator".into()),
                ..ActorContext::default()
            })
            .with_trusted_proxy_bearer("proxy-bearer-token");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer proxy-bearer-token")
                    .header("X-Memory-Actor-User", "alice,bob")
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
                .with_trusted_proxy_bearer("proxy-bearer-token"),
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

    /// A proxy's OIDC identity replaces the root actor template completely.
    #[tokio::test]
    async fn proxy_oidc_identity_drops_root_template_and_carries_the_pair() {
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(ActorContext {
                user: Some("root-operator".into()),
                email: Some("root@example.com".into()),
                ..ActorContext::default()
            })
            .with_trusted_proxy_bearer("proxy-bearer-token");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer proxy-bearer-token")
                    .header("X-Memory-Actor-Issuer", "https://idp.example")
                    .header("X-Memory-Actor-Sub", "oidc-subject-123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let actor = body_as_actor(resp).await;
        assert_eq!(actor.user, None, "the root username must not survive");
        assert_eq!(actor.issuer.as_deref(), Some("https://idp.example"));
        assert_eq!(actor.sub.as_deref(), Some("oidc-subject-123"));
        assert_eq!(actor.email, None);
    }

    /// Security boundary: the `X-Memory-Actor-*` headers are pure client input.
    /// With no proxy secret configured they must be ignored completely, or
    /// anyone who can reach the port authenticates as root and then names
    /// themselves whoever they like.
    #[tokio::test]
    async fn actor_headers_are_ignored_without_a_configured_proxy_bearer() {
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

    /// Root credentials never consume proxy assertion headers, even when the
    /// proxy rung is configured.
    #[tokio::test]
    async fn actor_headers_are_ignored_on_the_root_rung() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(root)
            .with_trusted_proxy_bearer("proxy-bearer-token");
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

    /// An unknown proxy bearer cannot fall through into the root rung.
    #[tokio::test]
    async fn wrong_proxy_bearer_is_unauthorized() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(root)
            .with_trusted_proxy_bearer("proxy-bearer-token");
        let resp = router_with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer wrong-proxy-token")
                    .header("X-Memory-Actor-User", "impostor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// The point of the feature: two people behind one proxy credential become
    /// different actors.
    #[tokio::test]
    async fn trusted_proxy_identity_builds_an_independent_actor() {
        let root = ActorContext {
            user: Some("boss".into()),
            email: Some("boss@example.com".into()),
            name: Some("Boss".into()),
            ..ActorContext::default()
        };
        let state = Arc::new(
            AuthState::new(Some("the-root-token".into()))
                .with_root_actor(root)
                .with_trusted_proxy_bearer("proxy-bearer-token"),
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
                        .header("Authorization", "Bearer proxy-bearer-token")
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

    /// A blank proxy bearer must not enable the trusted rung.
    #[tokio::test]
    async fn blank_proxy_bearer_does_not_enable_the_proxy_rung() {
        let root = ActorContext {
            user: Some("boss".into()),
            ..ActorContext::default()
        };
        let state = AuthState::new(Some("the-root-token".into()))
            .with_root_actor(root)
            .with_trusted_proxy_bearer("   ");
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
        let mut new_user = NewUser {
            username: username.into(),
            name: Some(format!("{username} display")),
            email: Some(format!("{username}@example.com")),
        };
        new_user.validate().unwrap();
        let user_id = store
            .writer
            .create_human_user(new_user, ai_memory_core::UserRole::User, None, false)
            .await
            .unwrap();
        let token = ai_memory_store::generate_api_key().unwrap();
        let token_hash = ai_memory_store::hash_token(&token, &pepper);
        store
            .writer
            .create_api_credential(
                ai_memory_core::ApiCredentialId::new(),
                user_id,
                "test".into(),
                token_hash,
                Some(ai_memory_store::api_key_preview(&token)),
            )
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
    async fn rung2_revoked_api_credential_is_rejected() {
        // Revoking a native API key must immediately stop authenticating
        // (no 30s cache window or similar). Critical for `ai-memory
        // api-key revoke` to be useful as an offboarding tool.
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
        let creds = state
            .multiuser
            .as_ref()
            .unwrap()
            .reader
            .list_api_credentials_for_user(user.id)
            .await
            .unwrap();
        let writer = state.multiuser.as_ref().unwrap().writer.clone();
        writer.revoke_api_credential(creds[0].id).await.unwrap();

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
    async fn rung2_rotated_api_credential_replaces_the_secret() {
        let (_tmp, state, old_token) = setup_multiuser("alice").await;
        let user = state
            .multiuser
            .as_ref()
            .unwrap()
            .reader
            .find_user_by_username("alice".into())
            .await
            .unwrap()
            .unwrap();
        let creds = state
            .multiuser
            .as_ref()
            .unwrap()
            .reader
            .list_api_credentials_for_user(user.id)
            .await
            .unwrap();
        let pepper = state.pepper().expect("pepper");
        let new_token = ai_memory_store::generate_api_key().unwrap();
        let writer = state.multiuser.as_ref().unwrap().writer.clone();
        writer
            .rotate_api_credential(
                creds[0].id,
                ai_memory_store::hash_token(&new_token, pepper),
                Some(ai_memory_store::api_key_preview(&new_token)),
            )
            .await
            .unwrap();

        let r = router_with_state(state);
        let old = r
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", format!("Bearer {old_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(old.status(), StatusCode::UNAUTHORIZED);
        let new = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", format!("Bearer {new_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(new.status(), StatusCode::OK);
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

        let mut user = NewUser {
            username: "alice".into(),
            name: None,
            email: None,
        };
        user.validate().unwrap();
        let user_id = store
            .writer
            .create_human_user(user, ai_memory_core::UserRole::User, None, false)
            .await
            .unwrap();
        let token = ai_memory_store::generate_api_key().unwrap();
        store
            .writer
            .create_api_credential(
                ai_memory_core::ApiCredentialId::new(),
                user_id,
                "test".into(),
                ai_memory_store::hash_token(&token, &pepper),
                Some(ai_memory_store::api_key_preview(&token)),
            )
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
    async fn require_bearer_401_advertises_bearer_not_basic() {
        let resp = router_with_auth(Some("secret"))
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .expect("WWW-Authenticate")
            .to_str()
            .unwrap();
        assert!(www.contains("Bearer"), "{www}");
        assert!(!www.to_ascii_lowercase().contains("basic"), "{www}");
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
