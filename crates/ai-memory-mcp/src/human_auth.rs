//! Human authentication: passwords, web sessions, CSRF, recovery.
//!
//! Four credential classes stay isolated here. Human passwords only
//! issue sessions. Sessions never authenticate `/mcp`, hooks, or
//! workstreams. Recovery never issues a session. Native API keys are
//! handled by [`crate::auth::require_bearer`] / dual-auth Bearer.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ai_memory_core::{
    ActorContext, AuthLevel, Capability, User, UserId, UserRole, validate_human_password,
};
use ai_memory_store::{LiveWebSession, ReaderPool, WriterHandle, hash_session_secret, hash_token};
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::auth::AuthState;

/// HttpOnly session cookie.
pub const SESSION_COOKIE: &str = "ai_memory_session";
/// Readable CSRF cookie.
pub const CSRF_COOKIE: &str = "ai_memory_csrf";
/// Deprecated Basic-auth compatibility cookie. Issued only before human auth
/// activates, then expired on the next browser response.
pub const LEGACY_AUTH_COOKIE: &str = "ai_memory_auth";
/// CSRF header required on cookie-authenticated mutations.
pub const CSRF_HEADER: &str = "x-csrf-token";

const LOGIN_WINDOW: Duration = Duration::from_secs(60);
const LOGIN_IP_LIMIT: usize = 10;
const LOGIN_LIMITER_MAX_KEYS: usize = 4_096;

/// Runtime extras for human auth. Attached to [`AuthState`].
#[derive(Clone)]
pub struct HumanAuthRuntime {
    /// Store reader.
    pub reader: ReaderPool,
    /// Store writer.
    pub writer: WriterHandle,
    /// SHA-256 of the break-glass recovery secret.
    pub recovery_token_hash: Option<[u8; 32]>,
    /// Recovery / bootstrap target username.
    pub root_username: String,
    /// Optional display fields for bootstrap/recovery create.
    pub root_name: Option<String>,
    /// Optional email for bootstrap/recovery create.
    pub root_email: Option<String>,
    /// Plaintext secrets that must not be chosen as a human password.
    pub reserved_passwords: Vec<String>,
    /// CIDRs allowed to supply `X-Forwarded-For`.
    pub trusted_proxy_cidrs: Vec<Cidr>,
    /// Login rate limiter.
    pub limiter: Arc<LoginLimiter>,
}

/// IPv4/IPv6 CIDR.
#[derive(Debug, Clone, Copy)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parse `a.b.c.d/n` or a bare address (`/32` or `/128`).
    ///
    /// # Errors
    /// Malformed address or prefix.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("empty CIDR".into());
        }
        let (addr_s, prefix) = if let Some((a, p)) = spec.split_once('/') {
            let prefix: u8 = p
                .parse()
                .map_err(|_| format!("invalid CIDR prefix in {spec:?}"))?;
            (a, prefix)
        } else if spec.contains(':') {
            (spec, 128)
        } else {
            (spec, 32)
        };
        let addr: IpAddr = addr_s
            .parse()
            .map_err(|_| format!("invalid CIDR address in {spec:?}"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(format!("CIDR prefix {prefix} exceeds {max}"));
        }
        Ok(Self { addr, prefix })
    }

    /// Membership test.
    #[must_use]
    pub fn contains(self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(got)) => {
                let shift = 32u32.saturating_sub(u32::from(self.prefix));
                let mask = if shift >= 32 { 0 } else { u32::MAX << shift };
                (u32::from(net) & mask) == (u32::from(got) & mask)
            }
            (IpAddr::V6(net), IpAddr::V6(got)) => {
                let shift = 128u32.saturating_sub(u32::from(self.prefix));
                let mask = if shift >= 128 { 0 } else { u128::MAX << shift };
                (u128::from(net) & mask) == (u128::from(got) & mask)
            }
            _ => false,
        }
    }
}

/// Bounded per-IP and per-username login attempt windows.
#[derive(Debug, Default)]
pub struct LoginLimiter {
    ip: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
    user: Mutex<HashMap<[u8; 32], VecDeque<Instant>>>,
}

impl LoginLimiter {
    fn prune(q: &mut VecDeque<Instant>, now: Instant) {
        while q
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > LOGIN_WINDOW)
        {
            q.pop_front();
        }
    }

    fn record_failure<K>(map: &mut HashMap<K, VecDeque<Instant>>, key: K, now: Instant)
    where
        K: Copy + Eq + std::hash::Hash,
    {
        if !map.contains_key(&key) && map.len() >= LOGIN_LIMITER_MAX_KEYS {
            map.retain(|_, q| {
                Self::prune(q, now);
                !q.is_empty()
            });
            if map.len() >= LOGIN_LIMITER_MAX_KEYS
                && let Some(oldest) = map
                    .iter()
                    .filter_map(|(key, q)| q.front().map(|instant| (*key, *instant)))
                    .min_by_key(|(_, instant)| *instant)
                    .map(|(key, _)| key)
            {
                map.remove(&oldest);
            }
        }
        let q = map.entry(key).or_default();
        Self::prune(q, now);
        q.push_back(now);
    }

    /// True when this IP has already spent its window. Call before Argon2.
    #[must_use]
    pub fn ip_blocked(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut map = self.ip.lock().unwrap_or_else(|e| e.into_inner());
        let Some(q) = map.get_mut(&ip) else {
            return false;
        };
        Self::prune(q, now);
        let blocked = q.len() >= LOGIN_IP_LIMIT;
        if q.is_empty() {
            map.remove(&ip);
        }
        blocked
    }

    /// Record an IP failure.
    pub fn record_ip_failure(&self, ip: IpAddr) {
        let now = Instant::now();
        let mut map = self.ip.lock().unwrap_or_else(|e| e.into_inner());
        Self::record_failure(&mut map, ip, now);
    }

    /// Record a username failure in a fixed-size, bounded key space.
    pub fn record_username_failure(&self, username: &str) {
        let now = Instant::now();
        let key = hash_session_secret(username);
        let mut map = self.user.lock().unwrap_or_else(|e| e.into_inner());
        Self::record_failure(&mut map, key, now);
    }
}

/// Public login + recovery (no session, no Bearer).
pub fn public_auth_router(state: Arc<AuthState>) -> Router {
    Router::new()
        .route("/auth/login", post(handle_login))
        .route("/auth/recovery", post(handle_recovery))
        .with_state(state)
}

/// Session-class `/auth/me`, password change, logout.
pub fn session_auth_router(state: Arc<AuthState>) -> Router {
    Router::new()
        .route("/auth/me", get(handle_me))
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_session_or_anonymous,
        ))
        .merge(
            Router::new()
                .route("/auth/password", post(handle_change_password))
                .route("/auth/logout", post(handle_logout))
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    require_session,
                )),
        )
        .with_state(state)
}

/// Private sidecar introspection. Handler checks the actor-proxy bearer.
pub fn internal_auth_router(state: Arc<AuthState>) -> Router {
    Router::new()
        .route(
            "/internal/auth/session-introspect",
            post(handle_session_introspect),
        )
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct LoginBody {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct RecoveryBody {
    recovery_token: String,
    new_password: String,
    new_password_confirmation: String,
}

#[derive(Debug, Deserialize)]
struct ChangePasswordBody {
    current_password: String,
    new_password: String,
    new_password_confirmation: String,
}

#[derive(Debug, Deserialize)]
struct IntrospectBody {
    session: String,
    method: String,
    #[serde(default)]
    csrf: Option<String>,
}

#[derive(Debug, Serialize)]
struct MeBody {
    username: Option<String>,
    name: Option<String>,
    role: Option<UserRole>,
    must_change_password: bool,
    via: &'static str,
    capabilities: CapabilitiesBody,
}

#[derive(Debug, Serialize)]
struct CapabilitiesBody {
    normal_read: bool,
    normal_write: bool,
    admin: bool,
    user_management: bool,
}

fn capabilities_for(level: AuthLevel, must_change: bool) -> CapabilitiesBody {
    let distinguishes = matches!(level, AuthLevel::Root | AuthLevel::User);
    let admin_ok = !must_change && level.authorize(Capability::Admin, distinguishes).is_ok();
    let users_ok = !must_change
        && level
            .authorize(Capability::UserManagement, distinguishes)
            .is_ok();
    CapabilitiesBody {
        normal_read: !must_change
            && level
                .authorize(Capability::NormalRead, distinguishes)
                .is_ok(),
        normal_write: !must_change
            && level
                .authorize(Capability::NormalWrite, distinguishes)
                .is_ok(),
        admin: admin_ok,
        user_management: users_ok,
    }
}

fn reserved_refs(runtime: &HumanAuthRuntime) -> Vec<&str> {
    runtime
        .reserved_passwords
        .iter()
        .map(String::as_str)
        .collect()
}

fn me_snapshot(user: &User) -> MeBody {
    let level = if user.role == UserRole::Root {
        AuthLevel::Root
    } else {
        AuthLevel::User
    };
    MeBody {
        username: Some(user.username.clone()),
        name: user.name.clone(),
        role: Some(user.role),
        must_change_password: user.must_change_password,
        via: "session",
        capabilities: capabilities_for(level, user.must_change_password),
    }
}

fn json_err(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

fn peer_ip(headers: &HeaderMap, extensions: &axum::http::Extensions, cidrs: &[Cidr]) -> IpAddr {
    let peer = extensions
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let Some(peer) = peer else {
        return IpAddr::from([0, 0, 0, 0]);
    };
    if !cidrs.iter().any(|c| c.contains(peer)) {
        return peer;
    }

    let forwarded = headers.get_all("x-forwarded-for");
    let mut hops = Vec::new();
    let mut found = false;
    for value in forwarded.iter() {
        found = true;
        let Ok(value) = value.to_str() else {
            return peer;
        };
        for hop in value.split(',') {
            let Ok(ip) = hop.trim().parse() else {
                return peer;
            };
            hops.push(ip);
        }
    }
    if !found {
        return peer;
    }
    hops.iter()
        .rev()
        .find(|ip| !cidrs.iter().any(|c| c.contains(**ip)))
        .copied()
        .or_else(|| hops.first().copied())
        .unwrap_or(peer)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let h = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in h.split(';') {
        let pair = pair.trim();
        if let Some(val) = pair.strip_prefix(&format!("{name}=")) {
            return Some(val.to_string());
        }
    }
    None
}

fn legacy_basic_password(headers: &HeaderMap) -> Option<String> {
    use base64::Engine;
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once([' ', '\t'])?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let credentials = std::str::from_utf8(&decoded).ok()?;
    let (_, password) = credentials.split_once(':')?;
    Some(password.to_string())
}

fn legacy_browser_credential(headers: &HeaderMap) -> Option<(String, bool)> {
    legacy_basic_password(headers)
        .map(|token| (token, true))
        .or_else(|| cookie_value(headers, LEGACY_AUTH_COOKIE).map(|token| (token, false)))
}

fn legacy_cookie(token: &str, secure: bool) -> String {
    set_cookie(LEGACY_AUTH_COOKIE, token, true, 2_592_000, secure)
}

fn unauthorized_legacy_browser() -> Response {
    let mut response = crate::auth::unauthorized_bearer();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        "Basic realm=\"ai-memory\", Bearer realm=\"ai-memory\", error=\"invalid_token\""
            .parse()
            .expect("static challenge is a valid header value"),
    );
    response
}

async fn human_auth_configured(state: &AuthState) -> Result<bool, Response> {
    let Some(runtime) = state.human.as_ref() else {
        return Ok(false);
    };
    if runtime
        .reader
        .bootstrap_completed()
        .await
        .map_err(|error| json_err(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()))?
    {
        return Ok(true);
    }
    runtime
        .reader
        .any_password_hash()
        .await
        .map_err(|error| json_err(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()))
}

fn set_cookie(name: &str, value: &str, http_only: bool, max_age: i64, secure: bool) -> String {
    let http = if http_only { "; HttpOnly" } else { "" };
    let sec = if secure { "; Secure" } else { "" };
    format!("{name}={value}{http}; SameSite=Strict; Path=/; Max-Age={max_age}{sec}")
}

fn expire_legacy(secure: bool) -> String {
    set_cookie(LEGACY_AUTH_COOKIE, "", true, 0, secure)
}

fn seconds_until_expiry(expires_at: i64) -> i64 {
    expires_at
        .saturating_sub(jiff::Timestamp::now().as_microsecond())
        .div_euclid(1_000_000)
        .max(0)
}

fn append_session_cookies(
    resp: &mut Response,
    session: &str,
    csrf: &str,
    expires_at: i64,
    secure: bool,
) {
    let headers = resp.headers_mut();
    let max_age = seconds_until_expiry(expires_at);
    if let Ok(v) = set_cookie(SESSION_COOKIE, session, true, max_age, secure).parse() {
        headers.append(header::SET_COOKIE, v);
    }
    if let Ok(v) = set_cookie(CSRF_COOKIE, csrf, false, max_age, secure).parse() {
        headers.append(header::SET_COOKIE, v);
    }
    if let Ok(v) = expire_legacy(secure).parse() {
        headers.append(header::SET_COOKIE, v);
    }
}

fn attach_legacy_expire(resp: &mut Response, secure: bool) {
    if let Ok(v) = expire_legacy(secure).parse() {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
}

/// Expire the deprecated Basic-auth cookie once human auth is active.
pub async fn expire_legacy_cookie_mw(
    State(state): State<Arc<AuthState>>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let expire = human_auth_configured(&state).await.unwrap_or(true);
    let mut resp = next.run(req).await;
    if expire {
        attach_legacy_expire(&mut resp, state.secure_cookie());
    }
    resp
}

fn clear_session_cookies(resp: &mut Response, secure: bool) {
    let headers = resp.headers_mut();
    if let Ok(v) = set_cookie(SESSION_COOKIE, "", true, 0, secure).parse() {
        headers.append(header::SET_COOKIE, v);
    }
    if let Ok(v) = set_cookie(CSRF_COOKIE, "", false, 0, secure).parse() {
        headers.append(header::SET_COOKIE, v);
    }
    if let Ok(v) = expire_legacy(secure).parse() {
        headers.append(header::SET_COOKIE, v);
    }
}

fn is_reserved_password(runtime: &HumanAuthRuntime, password: &str) -> bool {
    runtime
        .reserved_passwords
        .iter()
        .any(|s| !s.is_empty() && bool::from(s.as_bytes().ct_eq(password.as_bytes())))
        || runtime.recovery_token_hash.is_some_and(|expected| {
            bool::from(
                hash_session_secret(password)
                    .as_slice()
                    .ct_eq(expected.as_slice()),
            )
        })
}

async fn password_collides_with_api_key(
    runtime: &HumanAuthRuntime,
    pepper: Option<&ai_memory_store::TokenPepper>,
    password: &str,
) -> Result<bool, Response> {
    let Some(pepper) = pepper else {
        return Ok(false);
    };
    let hash = hash_token(password, pepper);
    runtime
        .reader
        .token_hash_exists(hash)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))
}

async fn issue_cookies(
    runtime: &HumanAuthRuntime,
    user_id: UserId,
    expected_phc: &str,
    expected_role: UserRole,
    expected_must_change: bool,
) -> Result<(String, String, i64), Response> {
    let secret = ai_memory_store::web_sessions::generate_session_secret()
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let csrf = ai_memory_store::web_sessions::generate_csrf_secret()
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?;
    let session_hash = hash_session_secret(&secret);
    let csrf_hash = hash_session_secret(&csrf);
    let issued = runtime
        .writer
        .issue_web_session(
            user_id,
            expected_phc.to_string(),
            expected_role,
            expected_must_change,
            session_hash,
            csrf_hash,
        )
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, user_id = %user_id, "session issuance rejected after password verification");
            json_err(StatusCode::UNAUTHORIZED, "invalid credentials")
        })?;
    Ok((secret, csrf, issued.expires_at))
}

async fn dummy_login_failure(
    runtime: &HumanAuthRuntime,
    ip: IpAddr,
    username: &str,
    password: String,
) -> Response {
    match ai_memory_store::password::dummy_verify(password).await {
        Err(ai_memory_store::StoreError::InvalidState(msg)) if msg.contains("saturated") => {
            json_err(StatusCode::TOO_MANY_REQUESTS, "kdf saturated")
        }
        Err(error) => {
            tracing::error!(%error, "dummy password verification failed");
            json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "authentication unavailable",
            )
        }
        Ok(()) => {
            runtime.limiter.record_ip_failure(ip);
            runtime.limiter.record_username_failure(username);
            json_err(StatusCode::UNAUTHORIZED, "invalid credentials")
        }
    }
}

async fn handle_login(
    State(state): State<Arc<AuthState>>,
    req: Request<axum::body::Body>,
) -> Response {
    let Some(runtime) = state.human.as_ref() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth required");
    };
    let headers = req.headers().clone();
    let ip = peer_ip(&headers, req.extensions(), &runtime.trusted_proxy_cidrs);
    if runtime.limiter.ip_blocked(ip) {
        return json_err(StatusCode::TOO_MANY_REQUESTS, "too many login attempts");
    }
    let body = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "invalid body"),
    };
    let LoginBody { username, password } = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "invalid body"),
    };
    let username = username.trim().to_string();

    let login = match runtime
        .reader
        .find_login_user_by_username(username.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let fail = || {
        runtime.limiter.record_ip_failure(ip);
        runtime.limiter.record_username_failure(&username);
        json_err(StatusCode::UNAUTHORIZED, "invalid credentials")
    };

    let Some(login) = login else {
        return dummy_login_failure(runtime, ip, &username, password).await;
    };
    if login.user.disabled_at.is_some() {
        return dummy_login_failure(runtime, ip, &username, password).await;
    }
    let Some(phc) = login.password_hash.clone() else {
        return dummy_login_failure(runtime, ip, &username, password).await;
    };
    let ok = match ai_memory_store::password::verify_password(password, phc.clone()).await {
        Ok(v) => v,
        Err(ai_memory_store::StoreError::InvalidState(msg)) if msg.contains("saturated") => {
            return json_err(StatusCode::TOO_MANY_REQUESTS, "kdf saturated");
        }
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if !ok {
        return fail();
    }

    let (secret, csrf, expires_at) = match issue_cookies(
        runtime,
        login.user.id,
        &phc,
        login.user.role,
        login.user.must_change_password,
    )
    .await
    {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let snapshot = me_snapshot(&login.user);
    let mut resp = Json(snapshot).into_response();
    append_session_cookies(&mut resp, &secret, &csrf, expires_at, state.secure_cookie());
    resp
}

async fn recovery_public_fail(
    runtime: &HumanAuthRuntime,
    ip: IpAddr,
    password: String,
) -> Response {
    runtime.limiter.record_ip_failure(ip);
    match ai_memory_store::password::dummy_verify(password).await {
        Err(ai_memory_store::StoreError::InvalidState(msg)) if msg.contains("saturated") => {
            json_err(StatusCode::TOO_MANY_REQUESTS, "kdf saturated")
        }
        _ => json_err(StatusCode::UNAUTHORIZED, "invalid credentials"),
    }
}

fn recovery_token_matches(expected: Option<&[u8; 32]>, provided: &str) -> bool {
    let provided_hash = hash_session_secret(provided);
    expected.is_some_and(|expected| bool::from(provided_hash.as_slice().ct_eq(expected.as_slice())))
}

async fn handle_recovery(
    State(state): State<Arc<AuthState>>,
    req: Request<axum::body::Body>,
) -> Response {
    let generic = || json_err(StatusCode::UNAUTHORIZED, "invalid credentials");
    let Some(runtime) = state.human.as_ref() else {
        return generic();
    };
    let headers = req.headers().clone();
    let ip = peer_ip(&headers, req.extensions(), &runtime.trusted_proxy_cidrs);
    if runtime.limiter.ip_blocked(ip) {
        return json_err(StatusCode::TOO_MANY_REQUESTS, "too many login attempts");
    }
    let body = match axum::body::to_bytes(req.into_body(), 64 * 1024).await {
        Ok(b) => b,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "invalid body"),
    };
    let RecoveryBody {
        recovery_token,
        new_password,
        new_password_confirmation,
    } = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return json_err(StatusCode::BAD_REQUEST, "invalid body"),
    };
    if !recovery_token_matches(runtime.recovery_token_hash.as_ref(), &recovery_token) {
        return recovery_public_fail(runtime, ip, new_password).await;
    }
    if new_password != new_password_confirmation
        || is_reserved_password(runtime, &new_password)
        || validate_human_password(
            &new_password,
            Some(runtime.root_username.as_str()),
            &reserved_refs(runtime),
        )
        .is_err()
    {
        return recovery_public_fail(runtime, ip, new_password).await;
    }
    match password_collides_with_api_key(runtime, state.pepper(), &new_password).await {
        Ok(true) => return recovery_public_fail(runtime, ip, new_password).await,
        Ok(false) => {}
        Err(resp) => return resp,
    }
    let phc = match ai_memory_store::password::hash_password(new_password).await {
        Ok(h) => h,
        Err(ai_memory_store::StoreError::InvalidState(msg)) if msg.contains("saturated") => {
            return json_err(StatusCode::TOO_MANY_REQUESTS, "kdf saturated");
        }
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if let Err(e) = runtime
        .writer
        .recover_root(
            runtime.root_username.clone(),
            runtime.root_name.clone(),
            runtime.root_email.clone(),
            phc,
        )
        .await
    {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    tracing::info!(username = %runtime.root_username, "root recovered via break-glass");
    let mut resp = StatusCode::NO_CONTENT.into_response();
    if let Ok(v) = expire_legacy(state.secure_cookie()).parse() {
        resp.headers_mut().append(header::SET_COOKIE, v);
    }
    resp
}

async fn handle_me(
    State(state): State<Arc<AuthState>>,
    axum::Extension(level): axum::Extension<AuthLevel>,
    axum::Extension(actor): axum::Extension<ActorContext>,
    session: Option<axum::Extension<LiveWebSession>>,
) -> Response {
    let mut resp = if level == AuthLevel::Anonymous {
        Json(MeBody {
            username: None,
            name: None,
            role: None,
            must_change_password: false,
            via: "anonymous",
            capabilities: capabilities_for(AuthLevel::Anonymous, false),
        })
        .into_response()
    } else {
        let user = session.map(|s| s.0.user.clone());
        Json(MeBody {
            username: actor
                .user
                .clone()
                .or_else(|| user.as_ref().map(|u| u.username.clone())),
            name: actor
                .name
                .clone()
                .or_else(|| user.as_ref().and_then(|u| u.name.clone())),
            role: user.as_ref().map(|u| u.role),
            must_change_password: user.as_ref().is_some_and(|u| u.must_change_password),
            via: "session",
            capabilities: capabilities_for(
                level,
                user.as_ref().is_some_and(|u| u.must_change_password),
            ),
        })
        .into_response()
    };
    attach_legacy_expire(&mut resp, state.secure_cookie());
    resp
}

async fn handle_change_password(
    State(state): State<Arc<AuthState>>,
    axum::Extension(session): axum::Extension<LiveWebSession>,
    Json(body): Json<ChangePasswordBody>,
) -> Response {
    let Some(runtime) = state.human.as_ref() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth required");
    };
    if body.new_password != body.new_password_confirmation {
        return json_err(
            StatusCode::BAD_REQUEST,
            "password confirmation does not match",
        );
    }
    if let Err(e) = validate_human_password(
        &body.new_password,
        Some(session.user.username.as_str()),
        &reserved_refs(runtime),
    ) {
        return json_err(StatusCode::BAD_REQUEST, &e.to_string());
    }
    if is_reserved_password(runtime, &body.new_password) {
        return json_err(StatusCode::BAD_REQUEST, "password is reserved");
    }
    let login = match runtime
        .reader
        .find_login_user_by_username(session.user.username.clone())
        .await
    {
        Ok(Some(u)) => u,
        Ok(None) => return json_err(StatusCode::UNAUTHORIZED, "auth required"),
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let Some(phc) = login.password_hash.clone() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth required");
    };
    let ok = match ai_memory_store::password::verify_password(body.current_password, phc.clone())
        .await
    {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if !ok {
        return json_err(StatusCode::UNAUTHORIZED, "invalid credentials");
    }
    match password_collides_with_api_key(runtime, state.pepper(), &body.new_password).await {
        Ok(true) => return json_err(StatusCode::BAD_REQUEST, "password is reserved"),
        Ok(false) => {}
        Err(response) => return response,
    }
    let new_phc = match ai_memory_store::password::hash_password(body.new_password).await {
        Ok(h) => h,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let secret = match ai_memory_store::web_sessions::generate_session_secret() {
        Ok(s) => s,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let csrf = match ai_memory_store::web_sessions::generate_csrf_secret() {
        Ok(s) => s,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    if let Err(e) = runtime
        .writer
        .change_password(
            session.user.id,
            phc,
            new_phc,
            session.id,
            hash_session_secret(&secret),
            hash_session_secret(&csrf),
        )
        .await
    {
        return json_err(StatusCode::CONFLICT, &e.to_string());
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    append_session_cookies(
        &mut resp,
        &secret,
        &csrf,
        session.expires_at,
        state.secure_cookie(),
    );
    resp
}

async fn handle_logout(State(state): State<Arc<AuthState>>, headers: HeaderMap) -> Response {
    if let (Some(runtime), Some(secret)) =
        (state.human.as_ref(), cookie_value(&headers, SESSION_COOKIE))
    {
        let hash = hash_session_secret(&secret);
        let _ = runtime.writer.revoke_web_session(hash).await;
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    clear_session_cookies(&mut resp, state.secure_cookie());
    resp
}

#[derive(Debug, Serialize)]
struct IntrospectResponse {
    authenticated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    can_manage_api_keys: bool,
}

async fn handle_session_introspect(
    State(state): State<Arc<AuthState>>,
    headers: HeaderMap,
    Json(body): Json<IntrospectBody>,
) -> Response {
    if crate::auth::any_actor_header(&headers) {
        return json_err(
            StatusCode::BAD_REQUEST,
            "X-Memory-Actor-* headers are not allowed on /internal",
        );
    }
    let Some(expected) = state.actor_proxy_bearer() else {
        return json_err(StatusCode::UNAUTHORIZED, "auth required");
    };
    let provided = crate::auth::extract_bearer_from_headers(&headers).unwrap_or_default();
    if provided.len() != expected.len()
        || !bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
    {
        return json_err(StatusCode::UNAUTHORIZED, "auth required");
    }
    let Some(runtime) = state.human.as_ref() else {
        return Json(IntrospectResponse {
            authenticated: false,
            username: None,
            can_manage_api_keys: false,
        })
        .into_response();
    };
    let hash = hash_session_secret(&body.session);
    let live = match runtime.reader.find_live_session_by_hash(hash).await {
        Ok(v) => v,
        Err(e) => return json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let Some(live) = live else {
        return Json(IntrospectResponse {
            authenticated: false,
            username: None,
            can_manage_api_keys: false,
        })
        .into_response();
    };
    let method = body.method.to_ascii_uppercase();
    let mutating = matches!(method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE");
    if mutating {
        let Some(csrf) = body.csrf.as_deref() else {
            return Json(IntrospectResponse {
                authenticated: false,
                username: None,
                can_manage_api_keys: false,
            })
            .into_response();
        };
        let csrf_hash = hash_session_secret(csrf);
        if !ai_memory_store::users::constant_time_eq(&csrf_hash, &live.csrf_hash) {
            return Json(IntrospectResponse {
                authenticated: false,
                username: None,
                can_manage_api_keys: false,
            })
            .into_response();
        }
    }
    let can_manage = live.user.role == UserRole::Root
        && !live.user.must_change_password
        && live.user.disabled_at.is_none();
    Json(IntrospectResponse {
        authenticated: true,
        username: Some(live.user.username),
        can_manage_api_keys: can_manage,
    })
    .into_response()
}

/// Session-only: reject Bearer; require a live cookie session.
pub async fn require_session(
    State(state): State<Arc<AuthState>>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if crate::auth::extract_bearer_from_headers(req.headers()).is_some() {
        return json_err(StatusCode::UNAUTHORIZED, "auth required");
    }
    match load_session(&state, req.headers(), req.method()).await {
        Ok(live) => inject_session(&mut req, live),
        Err(resp) => return resp,
    }
    next.run(req).await
}

/// `/auth/me`: anonymous when nothing is configured; otherwise a session.
pub async fn require_session_or_anonymous(
    State(state): State<Arc<AuthState>>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if crate::auth::extract_bearer_from_headers(req.headers()).is_some() {
        return json_err(StatusCode::UNAUTHORIZED, "auth required");
    }
    if !state.enabled() {
        req.extensions_mut().insert(ActorContext::anonymous());
        req.extensions_mut().insert(AuthLevel::Anonymous);
        return next.run(req).await;
    }
    match load_session(&state, req.headers(), req.method()).await {
        Ok(live) => inject_session(&mut req, live),
        Err(resp) => return resp,
    }
    next.run(req).await
}

/// Dual-auth for `/admin`, `/api/v1`, and the builtin wiki.
///
/// Bearer always wins and never falls back. Before a human password or
/// bootstrap marker exists, deprecated Basic / `ai_memory_auth` credentials
/// remain valid for GET only. The persisted human-auth transition is read on
/// every browser request, so completing bootstrap closes both legacy
/// transports without a restart.
pub async fn require_dual_auth(
    State(state): State<Arc<AuthState>>,
    mut req: Request<axum::body::Body>,
    next: Next,
) -> Response {
    match crate::auth::authenticate_bearer(&state, &mut req).await {
        Ok(crate::auth::BearerAuth::Authenticated) => {
            req.extensions_mut().insert(state.clone());
            return next.run(req).await;
        }
        Err(resp) => return resp,
        Ok(crate::auth::BearerAuth::Rejected) => {
            return crate::auth::unauthorized_bearer();
        }
        Ok(crate::auth::BearerAuth::Absent) => {}
    }

    let human_configured = match human_auth_configured(&state).await {
        Ok(configured) => configured,
        Err(response) => return response,
    };
    if !human_configured {
        if !state.enabled() {
            req.extensions_mut().insert(ActorContext::anonymous());
            req.extensions_mut().insert(AuthLevel::Anonymous);
            return next.run(req).await;
        }
        if req.method() != Method::GET {
            return crate::auth::unauthorized_bearer();
        }
        let had_legacy_cookie = cookie_value(req.headers(), LEGACY_AUTH_COOKIE).is_some();
        let Some((provided, from_basic)) = legacy_browser_credential(req.headers()) else {
            return unauthorized_legacy_browser();
        };
        match crate::auth::authenticate_token(&state, &mut req, &provided, false).await {
            Ok(crate::auth::BearerAuth::Authenticated) => {
                req.extensions_mut().insert(state.clone());
                let mut response = next.run(req).await;
                if from_basic
                    && !had_legacy_cookie
                    && let Ok(cookie) = legacy_cookie(&provided, state.secure_cookie()).parse()
                {
                    response.headers_mut().append(header::SET_COOKIE, cookie);
                }
                return response;
            }
            Err(response) => return response,
            Ok(crate::auth::BearerAuth::Absent | crate::auth::BearerAuth::Rejected) => {
                return unauthorized_legacy_browser();
            }
        }
    }

    match load_session(&state, req.headers(), req.method()).await {
        Ok(live) => {
            if live.user.must_change_password {
                return json_err(StatusCode::FORBIDDEN, "password change required");
            }
            inject_session(&mut req, live);
            req.extensions_mut().insert(state.clone());
            next.run(req).await
        }
        Err(resp) => resp,
    }
}

fn inject_session(req: &mut Request<axum::body::Body>, live: LiveWebSession) {
    let level = if live.user.role == UserRole::Root {
        AuthLevel::Root
    } else {
        AuthLevel::User
    };
    let actor = ActorContext {
        user: Some(live.user.username.clone()),
        name: live.user.name.clone(),
        email: live.user.email.clone(),
        ..ActorContext::default()
    };
    req.extensions_mut().insert(live.user.id);
    req.extensions_mut().insert(actor);
    req.extensions_mut().insert(level);
    req.extensions_mut().insert(live);
}

async fn load_session(
    state: &AuthState,
    headers: &HeaderMap,
    method: &Method,
) -> Result<LiveWebSession, Response> {
    let Some(runtime) = state.human.as_ref() else {
        return Err(json_err(StatusCode::UNAUTHORIZED, "auth required"));
    };
    let Some(secret) = cookie_value(headers, SESSION_COOKIE) else {
        return Err(json_err(StatusCode::UNAUTHORIZED, "auth required"));
    };
    let hash = hash_session_secret(&secret);
    let live = runtime
        .reader
        .find_live_session_by_hash(hash)
        .await
        .map_err(|e| json_err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()))?
        .ok_or_else(|| json_err(StatusCode::UNAUTHORIZED, "auth required"))?;
    if live.user.disabled_at.is_some() {
        return Err(json_err(StatusCode::UNAUTHORIZED, "auth required"));
    }
    if !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        let header_csrf = headers
            .get(CSRF_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let cookie_csrf = cookie_value(headers, CSRF_COOKIE);
        let (Some(h), Some(c)) = (header_csrf, cookie_csrf) else {
            return Err(json_err(StatusCode::FORBIDDEN, "csrf required"));
        };
        if h.len() != c.len() || !bool::from(h.as_bytes().ct_eq(c.as_bytes())) {
            return Err(json_err(StatusCode::FORBIDDEN, "csrf required"));
        }
        let csrf_hash = hash_session_secret(&h);
        if !ai_memory_store::users::constant_time_eq(&csrf_hash, &live.csrf_hash) {
            return Err(json_err(StatusCode::FORBIDDEN, "csrf required"));
        }
    }
    let writer = runtime.writer.clone();
    let session_id = live.id;
    let last_used_at = jiff::Timestamp::now().as_microsecond();
    let user_id = live.user.id;
    tokio::spawn(async move {
        let _ = writer.touch_web_session(session_id, last_used_at).await;
        let _ = writer.touch_user_last_seen(user_id).await;
    });
    Ok(live)
}

/// Helper used by admin password hashing to reject reserved values.
pub fn password_is_reserved(state: &AuthState, password: &str) -> bool {
    state
        .human
        .as_ref()
        .is_some_and(|r| is_reserved_password(r, password))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthState;
    use ai_memory_core::{ActorContext, AuthLevel};
    use axum::http::{StatusCode, header};
    use tower::ServiceExt;

    #[test]
    fn cidr_v4_contains() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains("10.1.2.3".parse().unwrap()));
        assert!(!c.contains("11.0.0.1".parse().unwrap()));
    }

    #[test]
    fn distinct_connection_peers_have_independent_login_budgets() {
        let headers = HeaderMap::new();
        let mut first_extensions = axum::http::Extensions::new();
        first_extensions.insert(axum::extract::ConnectInfo(
            "192.0.2.10:41000".parse::<SocketAddr>().unwrap(),
        ));
        let mut second_extensions = axum::http::Extensions::new();
        second_extensions.insert(axum::extract::ConnectInfo(
            "192.0.2.11:41000".parse::<SocketAddr>().unwrap(),
        ));
        let first = peer_ip(&headers, &first_extensions, &[]);
        let second = peer_ip(&headers, &second_extensions, &[]);
        assert_ne!(first, second);

        let limiter = LoginLimiter::default();
        for _ in 0..LOGIN_IP_LIMIT {
            limiter.record_ip_failure(first);
        }
        assert!(limiter.ip_blocked(first));
        assert!(!limiter.ip_blocked(second));
    }

    #[test]
    fn forwarded_ip_requires_a_trusted_connection_peer() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "198.51.100.20".parse().unwrap());
        let mut extensions = axum::http::Extensions::new();
        extensions.insert(axum::extract::ConnectInfo(
            "10.0.0.4:41000".parse::<SocketAddr>().unwrap(),
        ));
        let trusted = [Cidr::parse("10.0.0.0/8").unwrap()];
        assert_eq!(
            peer_ip(&headers, &extensions, &trusted),
            "198.51.100.20".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            peer_ip(&headers, &extensions, &[]),
            "10.0.0.4".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn forwarded_ip_combines_duplicate_field_lines_in_wire_order() {
        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", "198.51.100.99".parse().unwrap());
        headers.append("x-forwarded-for", "203.0.113.25".parse().unwrap());
        let mut extensions = axum::http::Extensions::new();
        extensions.insert(axum::extract::ConnectInfo(
            "10.0.0.4:41000".parse::<SocketAddr>().unwrap(),
        ));
        let trusted = [Cidr::parse("10.0.0.0/8").unwrap()];
        assert_eq!(
            peer_ip(&headers, &extensions, &trusted),
            "203.0.113.25".parse::<IpAddr>().unwrap()
        );

        headers.append("x-forwarded-for", "not-an-address".parse().unwrap());
        assert_eq!(
            peer_ip(&headers, &extensions, &trusted),
            "10.0.0.4".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn login_limiter_state_has_a_hard_global_cap() {
        let limiter = LoginLimiter::default();
        for n in 0..(LOGIN_LIMITER_MAX_KEYS + 50) {
            limiter.record_ip_failure(IpAddr::V6(std::net::Ipv6Addr::from(n as u128)));
            limiter.record_username_failure(&format!("attacker-controlled-{n}"));
        }
        assert!(
            limiter.ip.lock().unwrap_or_else(|e| e.into_inner()).len() <= LOGIN_LIMITER_MAX_KEYS
        );
        assert!(
            limiter.user.lock().unwrap_or_else(|e| e.into_inner()).len() <= LOGIN_LIMITER_MAX_KEYS
        );
    }

    #[test]
    fn rotated_cookie_age_never_exceeds_absolute_session_expiry() {
        let expires_at = jiff::Timestamp::now()
            .as_microsecond()
            .saturating_add(2_000_000);
        assert!((0..=2).contains(&seconds_until_expiry(expires_at)));
        assert_eq!(
            seconds_until_expiry(jiff::Timestamp::now().as_microsecond() - 1),
            0
        );
    }

    #[test]
    fn session_introspection_requires_csrf_on_post() {
        assert_eq!(CSRF_HEADER, "x-csrf-token");
        assert_eq!(SESSION_COOKIE, "ai_memory_session");
    }

    #[test]
    fn recovery_token_compare_hashes_before_constant_time_comparison() {
        let expected = hash_session_secret("abcd");
        assert!(recovery_token_matches(Some(&expected), "abcd"));
        assert!(!recovery_token_matches(Some(&expected), "ab"));
        assert!(!recovery_token_matches(Some(&expected), "abce"));
        assert!(!recovery_token_matches(None, "anything-at-all"));
    }

    async fn human_fixture(recovery: Option<&str>) -> (tempfile::TempDir, Arc<AuthState>, String) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ai_memory_store::Store::open(tmp.path()).unwrap();
        let password = "twelve-chars!!".to_string();
        let phc = ai_memory_store::password::hash_password(password.clone())
            .await
            .unwrap();
        let mut nu = ai_memory_core::NewUser {
            username: "root".into(),
            name: None,
            email: None,
        };
        nu.validate().unwrap();
        store
            .writer
            .create_human_user(nu, ai_memory_core::UserRole::Root, Some(phc), false)
            .await
            .unwrap();
        let runtime = HumanAuthRuntime {
            reader: store.reader.clone(),
            writer: store.writer.clone(),
            recovery_token_hash: recovery.map(hash_session_secret),
            root_username: "root".into(),
            root_name: None,
            root_email: None,
            reserved_passwords: vec!["root-bearer".into()],
            trusted_proxy_cidrs: Vec::new(),
            limiter: Arc::new(LoginLimiter::default()),
        };
        let state = Arc::new(
            AuthState::new(Some("root-bearer".into()))
                .with_root_actor(ActorContext {
                    user: Some("root".into()),
                    ..ActorContext::default()
                })
                .with_human(runtime),
        );
        (tmp, state, password)
    }

    fn set_cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
        for value in headers.get_all(header::SET_COOKIE) {
            let Ok(s) = value.to_str() else {
                continue;
            };
            let Some(rest) = s.strip_prefix(&format!("{name}=")) else {
                continue;
            };
            return Some(rest.split(';').next().unwrap_or("").to_string());
        }
        None
    }

    async fn login_cookies(state: Arc<AuthState>, password: &str) -> (String, String) {
        let resp = public_auth_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/auth/login")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "root",
                            "password": password
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers().clone();
        (
            set_cookie_value(&headers, SESSION_COOKIE).expect("session cookie"),
            set_cookie_value(&headers, CSRF_COOKIE).expect("csrf cookie"),
        )
    }

    async fn echo_level(axum::Extension(level): axum::Extension<AuthLevel>) -> &'static str {
        match level {
            AuthLevel::Root => "root",
            AuthLevel::User => "user",
            AuthLevel::Anonymous => "anonymous",
        }
    }

    fn dual_router(state: Arc<AuthState>) -> axum::Router {
        axum::Router::new()
            .route("/probe", axum::routing::get(echo_level).post(echo_level))
            .route_layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_dual_auth,
            ))
    }

    async fn json_error(resp: axum::http::Response<axum::body::Body>) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn cookie_pair(session: &str, csrf: &str) -> String {
        format!("{SESSION_COOKIE}={session}; {CSRF_COOKIE}={csrf}")
    }

    #[tokio::test]
    async fn dual_auth_basic_and_empty_authorization_use_session() {
        let (_tmp, state, password) = human_fixture(None).await;
        let (session, _csrf) = login_cookies(state.clone(), &password).await;
        let router = dual_router(state);
        for auth in [
            None,
            Some("Basic YWxpY2U6c2VjcmV0"),
            Some("Token not-a-bearer"),
        ] {
            let mut builder = axum::http::Request::builder()
                .uri("/probe")
                .header("cookie", format!("{SESSION_COOKIE}={session}"));
            if let Some(value) = auth {
                builder = builder.header("authorization", value);
            }
            let resp = router
                .clone()
                .oneshot(builder.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "auth={auth:?}");
            let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
            assert_eq!(&body[..], b"root");
        }
    }

    fn legacy_basic(token: &str) -> String {
        use base64::Engine;
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("ignored:{token}"))
        )
    }

    async fn legacy_transition_fixture() -> (
        tempfile::TempDir,
        Arc<AuthState>,
        ai_memory_store::WriterHandle,
    ) {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = ai_memory_store::Store::open(tmp.path()).unwrap();
        let writer = store.writer.clone();
        let runtime = HumanAuthRuntime {
            reader: store.reader.clone(),
            writer: store.writer.clone(),
            recovery_token_hash: Some(hash_session_secret("break-glass-recovery-token")),
            root_username: "root".into(),
            root_name: None,
            root_email: None,
            reserved_passwords: vec!["legacy-root-token".into()],
            trusted_proxy_cidrs: Vec::new(),
            limiter: Arc::new(LoginLimiter::default()),
        };
        let state = Arc::new(AuthState::new(Some("legacy-root-token".into())).with_human(runtime));
        (tmp, state, writer)
    }

    #[tokio::test]
    async fn legacy_browser_credentials_stop_when_human_bootstrap_completes() {
        let (_tmp, state, writer) = legacy_transition_fixture().await;
        let router = dual_router(state);

        let basic = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/probe")
                    .header("authorization", legacy_basic("legacy-root-token"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(basic.status(), StatusCode::OK);
        let legacy_cookie = set_cookie_value(basic.headers(), LEGACY_AUTH_COOKIE)
            .expect("successful Basic auth must persist the legacy browser cookie");

        let cookie = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/probe")
                    .header("cookie", format!("{LEGACY_AUTH_COOKIE}={legacy_cookie}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cookie.status(), StatusCode::OK);

        writer
            .bootstrap_root(
                "root".into(),
                None,
                None,
                "$argon2id$v=19$m=19456,t=2,p=1$ZmFrZQ$ZmFrZQ".into(),
            )
            .await
            .unwrap();

        for (name, authorization, cookie) in [
            ("basic", Some(legacy_basic("legacy-root-token")), None),
            (
                "cookie",
                None,
                Some(format!("{LEGACY_AUTH_COOKIE}={legacy_cookie}")),
            ),
        ] {
            let mut request = axum::http::Request::builder().uri("/probe");
            if let Some(value) = authorization {
                request = request.header("authorization", value);
            }
            if let Some(value) = cookie {
                request = request.header("cookie", value);
            }
            let response = router
                .clone()
                .oneshot(request.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{name} must fail immediately after human bootstrap"
            );
            assert!(
                response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|value| value.to_str().ok())
                    .is_none_or(|value| !value.contains("Basic")),
                "human mode must not advertise the legacy Basic challenge"
            );
        }
    }

    #[tokio::test]
    async fn dual_auth_invalid_bearer_does_not_fall_back_to_session() {
        let (_tmp, state, password) = human_fixture(None).await;
        let (session, _csrf) = login_cookies(state.clone(), &password).await;
        let resp = dual_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/probe")
                    .header("authorization", "Bearer not-a-known-token")
                    .header("cookie", format!("{SESSION_COOKIE}={session}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_me_anonymous_when_no_authority_configured() {
        let state = Arc::new(AuthState::new(None));
        let resp = session_auth_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/auth/me")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["via"], "anonymous");
        assert!(json["username"].is_null());
    }

    #[tokio::test]
    async fn auth_me_401_when_authority_configured_without_session() {
        let (_tmp, state, _password) = human_fixture(None).await;
        let resp = session_auth_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/auth/me")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn recovery_public_failures_share_generic_shape() {
        let recovery = "break-glass-recovery-token-32chr";
        let (_tmp, state, _password) = human_fixture(Some(recovery)).await;
        let cases = [
            serde_json::json!({
                "recovery_token": "wrong-recovery-token-value-xxxx",
                "new_password": "brand-new-pass!!",
                "new_password_confirmation": "brand-new-pass!!"
            }),
            serde_json::json!({
                "recovery_token": recovery,
                "new_password": "short",
                "new_password_confirmation": "short"
            }),
            serde_json::json!({
                "recovery_token": recovery,
                "new_password": "brand-new-pass!!",
                "new_password_confirmation": "does-not-match!!"
            }),
        ];
        for body in cases {
            let resp = public_auth_router(state.clone())
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/auth/recovery")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{body}");
            let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json, serde_json::json!({"error": "invalid credentials"}));
        }

        let (_tmp2, unset, _pw) = human_fixture(None).await;
        let resp = public_auth_router(unset)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/auth/recovery")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "recovery_token": recovery,
                            "new_password": "brand-new-pass!!",
                            "new_password_confirmation": "brand-new-pass!!"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json, serde_json::json!({"error": "invalid credentials"}));
    }

    #[tokio::test]
    async fn machine_require_bearer_ignores_session_cookie() {
        let (_tmp, state, password) = human_fixture(None).await;
        let (session, _csrf) = login_cookies(state.clone(), &password).await;
        let machine_only = [
            "/mcp",
            "/hook",
            "/hook/batch",
            "/handoff",
            "/workstream/runs",
        ];
        let mut router = axum::Router::new();
        for path in machine_only {
            router = router.route(path, axum::routing::get(|| async { "ok" }));
        }
        let router = router.layer(axum::middleware::from_fn_with_state(
            state,
            crate::auth::require_bearer,
        ));
        for path in machine_only {
            let resp = router
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(path)
                        .header("cookie", format!("{SESSION_COOKIE}={session}"))
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path}");
        }
    }

    #[tokio::test]
    async fn login_unknown_disabled_and_wrong_password_share_generic_shape() {
        let (_tmp, state, password) = human_fixture(None).await;
        let runtime = state.human.as_ref().unwrap();
        let phc = runtime
            .reader
            .find_login_user_by_username("root".into())
            .await
            .unwrap()
            .unwrap()
            .password_hash
            .unwrap();
        let mut alice = ai_memory_core::NewUser {
            username: "alice".into(),
            name: None,
            email: None,
        };
        alice.validate().unwrap();
        let alice_id = runtime
            .writer
            .create_human_user(alice, ai_memory_core::UserRole::User, Some(phc), false)
            .await
            .unwrap();
        runtime
            .writer
            .set_user_disabled(alice_id, true)
            .await
            .unwrap();

        let cases = [
            serde_json::json!({"username": "nobody", "password": &password}),
            serde_json::json!({"username": "alice", "password": &password}),
            serde_json::json!({"username": "root", "password": "definitely-wrong!!"}),
        ];
        for body in cases {
            let resp = public_auth_router(state.clone())
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri("/auth/login")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{body}");
            assert_eq!(
                json_error(resp).await,
                serde_json::json!({"error": "invalid credentials"})
            );
        }
    }

    #[tokio::test]
    async fn login_expires_legacy_auth_cookie_and_marks_session_http_only() {
        let (_tmp, state, password) = human_fixture(None).await;
        let resp = public_auth_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/auth/login")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "root",
                            "password": password
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let cookies: Vec<String> = resp
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_string))
            .collect();
        assert!(
            cookies.iter().any(|c| c.starts_with("ai_memory_session=")
                && c.contains("HttpOnly")
                && c.contains("SameSite=Strict")
                && c.contains("Path=/")),
            "{cookies:?}"
        );
        assert!(
            cookies
                .iter()
                .any(|c| c.starts_with("ai_memory_csrf=") && !c.contains("HttpOnly")),
            "{cookies:?}"
        );
        assert!(
            cookies
                .iter()
                .any(|c| c.starts_with("ai_memory_auth=") && c.contains("Max-Age=0")),
            "{cookies:?}"
        );
    }

    #[tokio::test]
    async fn dual_auth_post_requires_matching_csrf_header_and_cookie() {
        let (_tmp, state, password) = human_fixture(None).await;
        let (session, csrf) = login_cookies(state.clone(), &password).await;
        let router = dual_router(state.clone());

        let missing = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header("cookie", cookie_pair(&session, &csrf))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            json_error(missing).await,
            serde_json::json!({"error": "csrf required"})
        );

        let mismatched = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header("cookie", cookie_pair(&session, &csrf))
                    .header(CSRF_HEADER, "not-the-csrf-cookie-value")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(mismatched.status(), StatusCode::FORBIDDEN);

        let ok = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header("cookie", cookie_pair(&session, &csrf))
                    .header(CSRF_HEADER, &csrf)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let body = axum::body::to_bytes(ok.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"root");

        let bearer = dual_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/probe")
                    .header("authorization", "Bearer root-bearer")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bearer.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn must_change_password_forbids_dual_auth_console_routes() {
        let (_tmp, state, _password) = human_fixture(None).await;
        let runtime = state.human.as_ref().unwrap();
        let phc = runtime
            .reader
            .find_login_user_by_username("root".into())
            .await
            .unwrap()
            .unwrap()
            .password_hash
            .unwrap();
        let mut pending = ai_memory_core::NewUser {
            username: "pending".into(),
            name: None,
            email: None,
        };
        pending.validate().unwrap();
        runtime
            .writer
            .create_human_user(pending, ai_memory_core::UserRole::User, Some(phc), true)
            .await
            .unwrap();
        let resp = public_auth_router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/auth/login")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "pending",
                            "password": "twelve-chars!!"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let headers = resp.headers().clone();
        let session = set_cookie_value(&headers, SESSION_COOKIE).unwrap();
        let csrf = set_cookie_value(&headers, CSRF_COOKIE).unwrap();

        let blocked = dual_router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/probe")
                    .header("cookie", cookie_pair(&session, &csrf))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(blocked.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            json_error(blocked).await,
            serde_json::json!({"error": "password change required"})
        );

        let me = session_auth_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .uri("/auth/me")
                    .header("cookie", cookie_pair(&session, &csrf))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(me.status(), StatusCode::OK);
        let body = axum::body::to_bytes(me.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["must_change_password"], true);
        assert_eq!(json["username"], "pending");
    }

    #[tokio::test]
    async fn recovery_success_issues_no_session_and_revokes_the_old_one() {
        let recovery = "break-glass-recovery-token-32chr";
        let (_tmp, state, password) = human_fixture(Some(recovery)).await;
        let (session, csrf) = login_cookies(state.clone(), &password).await;
        let new_password = "brand-new-pass!!";
        let resp = public_auth_router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/auth/recovery")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "recovery_token": recovery,
                            "new_password": new_password,
                            "new_password_confirmation": new_password
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(set_cookie_value(resp.headers(), SESSION_COOKIE).is_none());
        assert!(
            resp.headers()
                .get_all(header::SET_COOKIE)
                .iter()
                .filter_map(|v| v.to_str().ok())
                .any(|c| c.starts_with("ai_memory_auth=") && c.contains("Max-Age=0")),
            "recovery must expire the legacy cookie"
        );

        let stale = session_auth_router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/auth/me")
                    .header("cookie", cookie_pair(&session, &csrf))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);

        let old = public_auth_router(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/auth/login")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "root",
                            "password": password
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(old.status(), StatusCode::UNAUTHORIZED);

        let fresh = public_auth_router(state)
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/auth/login")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "root",
                            "password": new_password
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(fresh.status(), StatusCode::OK);
        assert!(set_cookie_value(fresh.headers(), SESSION_COOKIE).is_some());
    }

    fn introspect_state(state: Arc<AuthState>) -> Arc<AuthState> {
        let human = state.human.clone().expect("human runtime");
        Arc::new(
            AuthState::new(Some("root-bearer".into()))
                .with_trusted_proxy_bearer("proxy-bearer-token")
                .with_human(human),
        )
    }

    #[tokio::test]
    async fn session_introspect_accepts_proxy_bearer_without_actor_headers() {
        let (_tmp, state, password) = human_fixture(None).await;
        let (session, csrf) = login_cookies(state.clone(), &password).await;
        let state = introspect_state(state);
        let router = internal_auth_router(state);

        let anonymous = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/auth/session-introspect")
                    .header("authorization", "Bearer proxy-bearer-token")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "session": "",
                            "method": "GET"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous.status(), StatusCode::OK);
        let body = axum::body::to_bytes(anonymous.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["authenticated"], false);
        assert_eq!(json["can_manage_api_keys"], false);

        let actor_rejected = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/auth/session-introspect")
                    .header("authorization", "Bearer proxy-bearer-token")
                    .header("x-memory-actor-user", "root")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "session": session,
                            "method": "GET"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(actor_rejected.status(), StatusCode::BAD_REQUEST);

        let live = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/auth/session-introspect")
                    .header("authorization", "Bearer proxy-bearer-token")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "session": session,
                            "method": "GET"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(live.status(), StatusCode::OK);
        let body = axum::body::to_bytes(live.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["authenticated"], true);
        assert_eq!(json["username"], "root");
        assert_eq!(json["can_manage_api_keys"], true);

        let mutating_without_csrf = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/auth/session-introspect")
                    .header("authorization", "Bearer proxy-bearer-token")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "session": session,
                            "method": "POST"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(mutating_without_csrf.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["authenticated"], false);

        let mutating_with_csrf = router
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/auth/session-introspect")
                    .header("authorization", "Bearer proxy-bearer-token")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "session": session,
                            "method": "POST",
                            "csrf": csrf
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(mutating_with_csrf.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["authenticated"], true);
        assert_eq!(json["can_manage_api_keys"], true);
    }
}
