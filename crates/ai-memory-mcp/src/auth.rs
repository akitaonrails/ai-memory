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
                // Every database user is subject to per-project access; an
                // open project admits them, a restricted one asks for a grant.
                req.extensions_mut()
                    .insert(ai_memory_core::AuthorizedViewer(hit.user.id));
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
        Ok(BearerAuth::Absent | BearerAuth::Rejected) if !state.enabled() => {
            // With no configured authority the wire gate is disabled. A
            // client may still carry a stale bearer from an older secured
            // deployment; treat it like an anonymous request instead of
            // making the no-auth server stricter than an absent header.
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
    async fn no_token_configured_ignores_an_unexpected_bearer() {
        let r = router_with_auth(None);
        let resp = r
            .oneshot(
                Request::builder()
                    .uri("/probe")
                    .header("Authorization", "Bearer stale-client-token")
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

    /// Every database user is subject to per-project access, so the
    /// middleware stamps the viewer for each of them — there is no switch.
    /// Root is never stamped: it is authorized above per-project granularity.
    #[tokio::test]
    async fn a_database_user_is_always_a_viewer_and_root_never_is() {
        async fn viewer(
            viewer: Option<axum::Extension<ai_memory_core::AuthorizedViewer>>,
        ) -> String {
            viewer.map_or_else(String::new, |axum::Extension(v)| v.user().to_string())
        }
        let (_tmp, state, token) = setup_multiuser("alice").await;
        let router = Router::new().route("/viewer", get(viewer)).layer(
            axum::middleware::from_fn_with_state(Arc::new(state), require_bearer),
        );
        let ask = |bearer: String| {
            let router = router.clone();
            async move {
                let resp = router
                    .oneshot(
                        Request::builder()
                            .uri("/viewer")
                            .header("Authorization", format!("Bearer {bearer}"))
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK);
                let bytes = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
                String::from_utf8(bytes.to_vec()).unwrap()
            }
        };
        assert!(!ask(token).await.is_empty(), "a database user is a viewer");
        assert!(
            ask("root-token-distinct-from-user-token".to_owned())
                .await
                .is_empty(),
            "root is not"
        );
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

    async fn owner_finish_user(store: &Store, name: &str) -> (ai_memory_core::UserId, String) {
        let id = store
            .writer
            .create_human_user(
                NewUser {
                    username: name.into(),
                    name: None,
                    email: None,
                },
                ai_memory_core::UserRole::User,
                None,
                false,
            )
            .await
            .unwrap();
        let token = ai_memory_store::generate_api_key().unwrap();
        store
            .writer
            .create_api_credential(
                ai_memory_core::ApiCredentialId::new(),
                id,
                "owner-finish".into(),
                ai_memory_store::hash_token(&token, &TokenPepper::new("finish-pepper")),
                None,
            )
            .await
            .unwrap();
        (id, token)
    }

    async fn owner_finish_run(
        store: &Store,
        ws: ai_memory_core::WorkspaceId,
        project: ai_memory_core::ProjectId,
        name: &str,
        owner: Option<String>,
    ) -> ai_memory_store::PreparedWorkstreamRun {
        store
            .writer
            .prepare_workstream_run_owned(
                ai_memory_store::PrepareWorkstreamRun {
                    workspace_id: ws,
                    project_id: project,
                    repo_fingerprint: "repo".into(),
                    worktree_fingerprint: "tree".into(),
                    cwd: "/repo".into(),
                    agent: ai_memory_core::AgentKind::Codex,
                    automatic_harness: false,
                    available_agents: Vec::new(),
                    selection: ai_memory_store::WorkstreamSelection::New(name.into()),
                    lease_owner: "diagnostic-only".into(),
                },
                owner,
            )
            .await
            .unwrap()
    }

    fn owner_finish_http(store: &Store, root: &std::path::Path, auth: AuthState) -> Router {
        ai_memory_hooks::workstream_router(ai_memory_hooks::WorkstreamState {
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            sanitizer: ai_memory_core::Sanitizer::default(),
            wiki: ai_memory_wiki::Wiki::new(root, store.writer.clone())
                .unwrap()
                .with_store_reader(store.reader.clone()),
            data_dir: root.into(),
            home_dir: None,
            trusted_proxy_identity: auth.actor_proxy_bearer().is_some(),
        })
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(auth),
            require_bearer,
        ))
    }

    async fn owner_finish_request(
        http: &Router,
        id: ai_memory_core::ManagedRunId,
        token: Option<&str>,
        events: serde_json::Value,
        complete: bool,
    ) -> Response {
        let mut request = Request::builder()
            .method("POST")
            .uri(format!("/workstream/runs/{id}/finish"))
            .header("Content-Type", "application/json")
            .header("X-Memory-Actor-User", "alice")
            .header("X-Memory-Actor-Sub", "alice")
            .header("X-Memory-Actor-Issuer", "https://idp.example");
        if let Some(token) = token {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let payload = serde_json::json!({
            "native_session_id": "native-owner", "source_cursor": "foreign-cursor",
            "events": events, "complete": complete, "exit_code": 9, "checkpoint": {},
            "actor": {"user": "alice"}, "principal": {"is_root": true}
        });
        let request = request.body(Body::from(payload.to_string())).unwrap();
        http.clone().oneshot(request).await.unwrap()
    }

    fn owner_finish_events() -> serde_json::Value {
        serde_json::json!([{
            "event_id": "owner-finish-event", "agent": "codex", "native_session_id": "native-owner",
            "kind": "message", "role": "assistant", "content": "owner finish evidence",
            "metadata": {"actor_user": "alice", "owner_user": "user:alice", "is_root": true}
        }])
    }

    async fn owner_finish_prepare_http(
        http: &Router,
        token: &str,
        name: &str,
    ) -> ai_memory_core::PrepareManagedRunResponse {
        let response = http
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/workstream/runs")
                    .header("Content-Type", "application/json")
                    .header("Authorization", format!("Bearer {token}"))
                    .header("X-Memory-Actor-User", "alice")
                    .header("X-Memory-Actor-Issuer", "https://idp.example")
                    .header("X-Memory-Actor-Sub", "alice")
                    .body(Body::from(
                        serde_json::json!({
                            "workspace": "finish-compat", "project": "restricted", "cwd": "/repo",
                            "repo_fingerprint": "compat-repo", "worktree_fingerprint": name,
                            "agent": "codex", "new_workstream": name, "lease_owner": "display-only"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "authenticated prepare must remain legitimate"
        );
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn owner_finish_http_trusted_proxy_preserves_restricted_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("finish-compat")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "restricted", None)
            .await
            .unwrap();
        store
            .writer
            .set_access_mode(project, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
        // A DB user with the same display name must not change the proxy's OIDC owner.
        let (alice, db_token) = owner_finish_user(&store, "alice").await;
        let auth = AuthState::new(Some("root-finish-token".into()))
            .with_trusted_proxy_bearer("proxy-finish-token")
            .with_multiuser(
                TokenPepper::new("finish-pepper"),
                store.reader.clone(),
                store.writer.clone(),
            );
        let http = owner_finish_http(&store, tmp.path(), auth.clone());
        let run = owner_finish_prepare_http(&http, "proxy-finish-token", "proxy-owned").await;
        let before = owner_finish_snapshot(&store).await;
        for token in [None, Some("invalid-token")] {
            assert_eq!(
                owner_finish_request(&http, run.run_id, token, owner_finish_events(), true)
                    .await
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        for invalid in [true, false] {
            let mut events = owner_finish_events();
            if invalid {
                events[0]["agent"] = serde_json::json!("claude-code");
            }
            let response = http.clone().oneshot(
                Request::builder().method("POST").uri(format!("/workstream/runs/{}/finish", run.run_id))
                    .header("Content-Type", "application/json")
                    .header("Authorization", "Bearer proxy-finish-token")
                    .header("X-Memory-Actor-User", "alice")
                    .header("X-Memory-Actor-Issuer", "https://idp.example")
                    .header("X-Memory-Actor-Sub", "bob")
                    .body(Body::from(serde_json::json!({"events": events, "complete": true, "checkpoint": {}, "actor": {"sub": "alice"}, "principal": {"is_root": true}}).to_string())).unwrap()
            ).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "foreign proxy owner must be refused before payload handling"
            );
        }
        let malformed = http
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/workstream/runs/{}/finish", run.run_id))
                    .header("Content-Type", "application/json")
                    .header("Authorization", "Bearer proxy-finish-token")
                    .header("X-Memory-Actor-Issuer", "https://idp.example")
                    .body(Body::from("{\"events\":[],\"complete\":true}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(owner_finish_snapshot(&store).await, before);
        // Enabling proxy authentication never exempts an actual DB principal from grants.
        let db_run = owner_finish_run(
            &store,
            ws,
            project,
            "db-no-grant",
            Some("user:alice".into()),
        )
        .await;
        let db_before = owner_finish_snapshot(&store).await;
        assert_eq!(
            owner_finish_request(
                &http,
                db_run.run_id,
                Some(&db_token),
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
            "a DB principal must retain its current project grant check with proxy enabled"
        );
        assert_eq!(owner_finish_snapshot(&store).await, db_before);
        assert!(!tmp.path().join("raw/workstreams").exists());
        // The configured server flag is also required, not just User without a DB id.
        let without_proxy_policy =
            ai_memory_hooks::workstream_router(ai_memory_hooks::WorkstreamState {
                writer: store.writer.clone(),
                reader: store.reader.clone(),
                sanitizer: ai_memory_core::Sanitizer::default(),
                wiki: ai_memory_wiki::Wiki::new(tmp.path(), store.writer.clone())
                    .unwrap()
                    .with_store_reader(store.reader.clone()),
                data_dir: tmp.path().into(),
                home_dir: None,
                trusted_proxy_identity: false,
            })
            .layer(axum::middleware::from_fn_with_state(
                Arc::new(auth),
                require_bearer,
            ));
        assert_eq!(
            owner_finish_request(
                &without_proxy_policy,
                run.run_id,
                Some("proxy-finish-token"),
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
            "proxy compatibility requires the server's trusted-proxy configuration"
        );
        assert_eq!(owner_finish_snapshot(&store).await, db_before);
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.execute(
            "UPDATE managed_runs SET lease_expires_at = 0 WHERE id = ?1",
            rusqlite::params![run.run_id.as_bytes()],
        )
        .unwrap();
        let response = owner_finish_request(
            &http,
            run.run_id,
            Some("proxy-finish-token"),
            owner_finish_events(),
            true,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "a genuine proxy owner must finish the restricted run it prepared"
        );
        let finished = owner_finish_snapshot(&store).await;
        let foreign_retry = http.clone().oneshot(
            Request::builder().method("POST").uri(format!("/workstream/runs/{}/finish", run.run_id))
                .header("Content-Type", "application/json")
                .header("Authorization", "Bearer proxy-finish-token")
                .header("X-Memory-Actor-User", "alice")
                .header("X-Memory-Actor-Issuer", "https://idp.example")
                .header("X-Memory-Actor-Sub", "bob")
                .body(Body::from(serde_json::json!({"events": owner_finish_events(), "complete": true, "checkpoint": {}}).to_string())).unwrap()
        ).await.unwrap();
        assert_eq!(
            foreign_retry.status(),
            StatusCode::FORBIDDEN,
            "finished proxy retries must still refuse a foreign owner"
        );
        assert_eq!(owner_finish_snapshot(&store).await, finished);
        let retry = owner_finish_request(
            &http,
            run.run_id,
            Some("proxy-finish-token"),
            owner_finish_events(),
            true,
        )
        .await;
        assert_eq!(retry.status(), StatusCode::OK);
        let body = axum::body::to_bytes(retry.into_body(), 65536)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["imported_events"],
            0
        );
        assert_eq!(owner_finish_snapshot(&store).await, finished);
        store
            .writer
            .grant_memory(alice, project, ai_memory_store::GrantLevel::Write, None)
            .await
            .unwrap();
        assert_eq!(
            owner_finish_request(
                &http,
                run.run_id,
                Some(&db_token),
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
            "a DB username cannot forge the proxy's qualified OIDC owner"
        );
        assert_eq!(owner_finish_snapshot(&store).await, finished);
        assert_eq!(
            owner_finish_request(
                &http,
                db_run.run_id,
                Some(&db_token),
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn owner_finish_http_active_api_key_survives_disabled_human_login() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("finish-compat")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "restricted", None)
            .await
            .unwrap();
        store
            .writer
            .set_access_mode(project, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
        let (alice, token) = owner_finish_user(&store, "alice").await;
        store
            .writer
            .grant_memory(alice, project, ai_memory_store::GrantLevel::Write, None)
            .await
            .unwrap();
        store.writer.set_user_disabled(alice, true).await.unwrap();
        let http = owner_finish_http(
            &store,
            tmp.path(),
            AuthState::new(Some("root-finish-token".into())).with_multiuser(
                TokenPepper::new("finish-pepper"),
                store.reader.clone(),
                store.writer.clone(),
            ),
        );
        let run = owner_finish_prepare_http(&http, &token, "api-human-disabled").await;
        let response =
            owner_finish_request(&http, run.run_id, Some(&token), owner_finish_events(), true)
                .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "an active API key with Write must finish after human login is disabled"
        );
        let revoked = owner_finish_prepare_http(&http, &token, "api-grant-revoked").await;
        store
            .writer
            .revoke_memory(alice, project, None)
            .await
            .unwrap();
        let before = owner_finish_snapshot(&store).await;
        let response = owner_finish_request(
            &http,
            revoked.run_id,
            Some(&token),
            owner_finish_events(),
            true,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "disabled human login does not waive current API-user project grants"
        );
        assert_eq!(owner_finish_snapshot(&store).await, before);
        assert!(
            !tmp.path()
                .join(format!(
                    "raw/workstreams/{}/segments",
                    revoked.workstream_id
                ))
                .exists()
        );
        let credentials = store
            .reader
            .list_api_credentials_for_user(alice)
            .await
            .unwrap();
        store
            .writer
            .revoke_api_credential(credentials[0].id)
            .await
            .unwrap();
        assert_eq!(
            owner_finish_request(&http, run.run_id, Some(&token), owner_finish_events(), true)
                .await
                .status(),
            StatusCode::UNAUTHORIZED,
            "a revoked API key must be refused by the real middleware"
        );
        assert_eq!(owner_finish_snapshot(&store).await, before);
    }

    async fn owner_finish_snapshot(store: &Store) -> Vec<Vec<rusqlite::types::Value>> {
        store
            .reader
            .with_conn(|conn| {
                let mut result = Vec::new();
                for table in [
                    "managed_runs",
                    "workstreams",
                    "workstream_events",
                    "workstream_native_sessions",
                    "workstream_events_fts_data",
                    "workstream_events_fts_idx",
                    "workstream_events_fts_docsize",
                    "sessions",
                ] {
                    let mut stmt = conn.prepare(&format!("SELECT * FROM {table} ORDER BY 1, 2"))?;
                    let count = stmt.column_count();
                    let rows =
                        stmt.query_map([], |row| (0..count).map(|i| row.get(i)).collect())?;
                    result.extend(
                        rows.collect::<rusqlite::Result<Vec<Vec<rusqlite::types::Value>>>>()?,
                    );
                }
                Ok(result)
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn owner_finish_http_refuses_another_writer_before_payload_or_segment() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("owner-finish")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "shared", None)
            .await
            .unwrap();
        let (alice, alice_token) = owner_finish_user(&store, "alice").await;
        let (bob, bob_token) = owner_finish_user(&store, "bob").await;
        store
            .writer
            .set_access_mode(project, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
        for user in [alice, bob] {
            store
                .writer
                .grant_memory(user, project, ai_memory_store::GrantLevel::Write, None)
                .await
                .unwrap();
        }
        let run = owner_finish_run(&store, ws, project, "owned", Some("user:alice".into())).await;
        let http = owner_finish_http(
            &store,
            tmp.path(),
            AuthState::new(Some("root-finish-token".into())).with_multiuser(
                TokenPepper::new("finish-pepper"),
                store.reader.clone(),
                store.writer.clone(),
            ),
        );
        let before = owner_finish_snapshot(&store).await;
        let mut invalid = owner_finish_events();
        invalid[0]["agent"] = serde_json::json!("claude-code");
        let response =
            owner_finish_request(&http, run.run_id, Some(&bob_token), invalid, true).await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "owner refusal must precede payload validation"
        );
        for complete in [false, true] {
            let response = owner_finish_request(
                &http,
                run.run_id,
                Some(&bob_token),
                owner_finish_events(),
                complete,
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::FORBIDDEN,
                "Write permission must not bypass another author's owner"
            );
        }
        assert_eq!(owner_finish_snapshot(&store).await, before);
        assert!(
            !tmp.path().join("raw/workstreams").exists(),
            "foreign owner must write no raw segment"
        );
        let response = owner_finish_request(
            &http,
            run.run_id,
            Some(&alice_token),
            owner_finish_events(),
            true,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let before = owner_finish_snapshot(&store).await;
        let response = owner_finish_request(
            &http,
            run.run_id,
            Some(&bob_token),
            owner_finish_events(),
            true,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "finished retries still check the owner"
        );
        let response = owner_finish_request(
            &http,
            run.run_id,
            Some(&alice_token),
            owner_finish_events(),
            true,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["imported_events"],
            0
        );
        assert_eq!(owner_finish_snapshot(&store).await, before);
    }

    #[tokio::test]
    async fn owner_finish_http_real_auth_rungs_shared_and_project_controls() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store.writer.get_or_create_workspace("rungs").await.unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "shared", None)
            .await
            .unwrap();
        let anonymous_http = owner_finish_http(&store, tmp.path(), AuthState::new(None));
        let anonymous_run = owner_finish_run(&store, ws, project, "anonymous", None).await;
        assert_eq!(
            owner_finish_request(
                &anonymous_http,
                anonymous_run.run_id,
                None,
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::OK
        );
        let (alice, token) = owner_finish_user(&store, "alice").await;
        store
            .writer
            .set_access_mode(project, ai_memory_store::AccessMode::Restricted)
            .await
            .unwrap();
        let http = owner_finish_http(
            &store,
            tmp.path(),
            AuthState::new(Some("root-finish-token".into())).with_multiuser(
                TokenPepper::new("finish-pepper"),
                store.reader.clone(),
                store.writer.clone(),
            ),
        );
        let shared = owner_finish_run(&store, ws, project, "shared-root", None).await;
        let before = owner_finish_snapshot(&store).await;
        assert_eq!(
            owner_finish_request(
                &anonymous_http,
                shared.run_id,
                None,
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
            "an anonymous caller must not acquire root on topology reload"
        );
        assert_eq!(
            owner_finish_request(&http, shared.run_id, None, owner_finish_events(), true)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        store
            .writer
            .grant_memory(alice, project, ai_memory_store::GrantLevel::Read, None)
            .await
            .unwrap();
        assert_eq!(
            owner_finish_request(
                &http,
                shared.run_id,
                Some(&token),
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
            "Read is insufficient for finish"
        );
        assert_eq!(owner_finish_snapshot(&store).await, before);
        assert_eq!(
            owner_finish_request(
                &http,
                shared.run_id,
                Some("root-finish-token"),
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::OK,
            "authenticated root without actor may finish NULL-owned runs"
        );
        store
            .writer
            .grant_memory(alice, project, ai_memory_store::GrantLevel::Write, None)
            .await
            .unwrap();
        let owned =
            owner_finish_run(&store, ws, project, "owned-user", Some("user:alice".into())).await;
        let before = owner_finish_snapshot(&store).await;
        assert_eq!(
            owner_finish_request(
                &http,
                owned.run_id,
                Some("root-finish-token"),
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::FORBIDDEN,
            "root without actor must not become Any"
        );
        assert_eq!(owner_finish_snapshot(&store).await, before);
        // Exercise the real DB-user tier with only its optional viewer marker removed.
        let without_viewer = ai_memory_hooks::workstream_router(ai_memory_hooks::WorkstreamState {
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            sanitizer: ai_memory_core::Sanitizer::default(),
            wiki: ai_memory_wiki::Wiki::new(tmp.path(), store.writer.clone())
                .unwrap()
                .with_store_reader(store.reader.clone()),
            data_dir: tmp.path().into(),
            home_dir: None,
            trusted_proxy_identity: false,
        })
        .layer(axum::middleware::from_fn(
            |mut request: Request<Body>, next: Next| async move {
                request
                    .extensions_mut()
                    .remove::<ai_memory_core::AuthorizedViewer>();
                next.run(request).await
            },
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(
                AuthState::new(Some("root-finish-token".into())).with_multiuser(
                    TokenPepper::new("finish-pepper"),
                    store.reader.clone(),
                    store.writer.clone(),
                ),
            ),
            require_bearer,
        ));
        let response = owner_finish_request(
            &without_viewer,
            owned.run_id,
            Some(&token),
            owner_finish_events(),
            true,
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "DB-user authority must survive an absent optional viewer marker"
        );
        for (workspace, name) in [
            (ws, "foreign-project"),
            (
                store
                    .writer
                    .get_or_create_workspace("other-rungs")
                    .await
                    .unwrap(),
                "shared",
            ),
        ] {
            let foreign = store
                .writer
                .get_or_create_project(workspace, name, None)
                .await
                .unwrap();
            store
                .writer
                .set_access_mode(foreign, ai_memory_store::AccessMode::Restricted)
                .await
                .unwrap();
            let run = owner_finish_run(&store, workspace, foreign, name, None).await;
            let before = owner_finish_snapshot(&store).await;
            assert_eq!(
                owner_finish_request(&http, run.run_id, Some(&token), owner_finish_events(), true)
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
            assert_eq!(owner_finish_snapshot(&store).await, before);
            assert!(
                !tmp.path()
                    .join("raw/workstreams")
                    .join(run.workstream_id.to_string())
                    .exists()
            );
            store
                .writer
                .grant_memory(alice, foreign, ai_memory_store::GrantLevel::Write, None)
                .await
                .unwrap();
            assert_eq!(
                owner_finish_request(&http, run.run_id, Some(&token), owner_finish_events(), true)
                    .await
                    .status(),
                StatusCode::OK
            );
        }
    }

    #[tokio::test]
    async fn owner_finish_http_preserves_late_active_terminal_and_missing_phases() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("phases")
            .await
            .unwrap();
        let project = store
            .writer
            .get_or_create_project(ws, "shared", None)
            .await
            .unwrap();
        let http = owner_finish_http(
            &store,
            tmp.path(),
            AuthState::new(Some("root-finish-token".into())),
        );
        let run = owner_finish_run(&store, ws, project, "late-active", None).await;
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.execute(
            "UPDATE managed_runs SET lease_expires_at = 0 WHERE id = ?1",
            [run.run_id.as_bytes()],
        )
        .unwrap();
        assert_eq!(
            owner_finish_request(
                &http,
                run.run_id,
                Some("root-finish-token"),
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::OK,
            "active is allowed even after the lease timestamp"
        );
        let before = owner_finish_snapshot(&store).await;
        let response = owner_finish_request(
            &http,
            run.run_id,
            Some("root-finish-token"),
            owner_finish_events(),
            true,
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["imported_events"],
            0
        );
        assert_eq!(owner_finish_snapshot(&store).await, before);
        let expired = owner_finish_run(&store, ws, project, "cancelled", None).await;
        assert!(
            store
                .writer
                .cancel_managed_run(expired.run_id)
                .await
                .unwrap()
        );
        let before = owner_finish_snapshot(&store).await;
        assert_eq!(
            owner_finish_request(
                &http,
                expired.run_id,
                Some("root-finish-token"),
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            owner_finish_request(
                &http,
                ai_memory_core::ManagedRunId::new(),
                Some("root-finish-token"),
                owner_finish_events(),
                true
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(owner_finish_snapshot(&store).await, before);
        assert!(
            !tmp.path()
                .join("raw/workstreams")
                .join(expired.workstream_id.to_string())
                .exists()
        );
    }
}
