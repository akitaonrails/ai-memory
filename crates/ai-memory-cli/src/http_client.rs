//! Shared HTTP-client glue for thin-client CLI subcommands.
//!
//! Every state-touching subcommand (status, search, bootstrap, …) goes
//! through these helpers so URL resolution + bearer-auth handling stays
//! consistent in one place.
//!
//! ## Configuration
//!
//! [`crate::config::Config`] captures `AI_MEMORY_SERVER_URL` and
//! `AI_MEMORY_AUTH_TOKEN` exactly once; this module consumes those values
//! and can fall back to the stored OIDC device-flow token used by native hooks.

use std::fmt;
use std::io::{BufWriter, Write as _};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::{Config, DEFAULT_SERVER_URL};
use ai_memory_web::normalize_prefix;

/// Non-success response returned by the configured ai-memory server.
#[derive(Debug)]
pub(crate) struct ServerResponseError {
    method: reqwest::Method,
    path: String,
    status: reqwest::StatusCode,
    body: String,
}

impl ServerResponseError {
    #[must_use]
    pub(crate) const fn status(&self) -> reqwest::StatusCode {
        self.status
    }

    #[must_use]
    pub(crate) fn body(&self) -> &str {
        &self.body
    }
}

impl fmt::Display for ServerResponseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}: server returned {}: {}",
            self.method, self.path, self.status, self.body
        )
    }
}

impl std::error::Error for ServerResponseError {}

fn server_response_error(
    method: reqwest::Method,
    url: &reqwest::Url,
    status: reqwest::StatusCode,
    body: String,
) -> anyhow::Error {
    ServerResponseError {
        method,
        path: url.path().to_owned(),
        status,
        body,
    }
    .into()
}

#[cfg(test)]
pub(crate) fn server_response_error_for_test(
    method: reqwest::Method,
    path: &str,
    status: reqwest::StatusCode,
    body: String,
) -> anyhow::Error {
    ServerResponseError {
        method,
        path: path.to_owned(),
        status,
        body,
    }
    .into()
}

/// Pass a 2xx response through, else consume the body into a
/// [`ServerResponseError`].
///
/// Only the request path reaches the error, so userinfo and query
/// credentials in the URL never land in a message. `reqwest` does not carry
/// the request method on the response, so callers supply it.
async fn require_success(
    method: reqwest::Method,
    resp: reqwest::Response,
) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let url = resp.url().clone();
    let body = resp.text().await.unwrap_or_default();
    Err(server_response_error(method, &url, status, body))
}

/// Resolved server target — origin URL + base-path prefix + optional bearer token.
#[derive(Debug, Clone)]
pub struct ServerEndpoint {
    /// Origin only (scheme + authority), any trailing slash and path
    /// stripped, e.g. `http://127.0.0.1:49374` or `http://192.168.0.90:49374`.
    pub url: String,
    /// Normalised base-path prefix the server is mounted under, e.g.
    /// `/wiki`, or empty when serving at the root. Always either empty or
    /// `/`-prefixed with no trailing slash (normalised by `normalize_prefix`).
    pub base_path: String,
    /// Bearer token when present, else `None`.
    pub auth_token: Option<String>,
    url_configured: bool,
}

impl ServerEndpoint {
    /// Build the endpoint from config, resolving bearer auth and base path.
    ///
    /// Bearer precedence matches hooks: static config/env token first, stored
    /// OIDC device token second, no token last.
    ///
    /// The base-path prefix the server is mounted under, so client routes
    /// resolve as `<origin><base><path>` instead of 404ing) is resolved
    /// from, in order of precedence:
    /// 1. the **path component of `AI_MEMORY_SERVER_URL`** (the remote-client
    ///    case — `http://host:49374/wiki` → origin `http://host:49374`,
    ///    base `/wiki`), then
    /// 2. **`Config::base_path`** (the in-pod case — figment populates the
    ///    field from `AI_MEMORY_BASE_PATH` inside `Config::load`, keeping the
    ///    "one config-read path" invariant; the CLI runs in the same
    ///    container as `serve`, which already reads the same env var via
    ///    clap to nest its router).
    pub async fn from_config_resolving_auth(config: &Config) -> Self {
        let client = reqwest::Client::new();
        let token = crate::auth_bearer::resolve_bearer(
            &client,
            &config.oidc_device_token_path(),
            config.auth.bearer_token.as_deref(),
        )
        .await;
        Self::build(
            Some(config.server_url.clone()),
            token,
            config.server_url_configured(),
            Some(config.base_path.clone()).filter(|s| !s.is_empty()),
        )
    }

    /// Build for a target the spawning hook already resolved: its URL
    /// (including any mount path, as `install-hooks` rendered it) and the
    /// bearer it authenticates with. No config or environment is consulted.
    #[must_use]
    pub(crate) fn for_hook_target(url: String, token: Option<String>) -> Self {
        Self::build(Some(url), token, true, None)
    }

    /// Build from an explicit URL + token pair (useful for tests that
    /// cannot safely mutate the process environment).
    ///
    /// `url` defaults to `http://127.0.0.1:49374` when `None` or empty;
    /// trailing slashes are stripped. `token` is treated as absent when
    /// `None` or empty. No environment is read — the env base-path fallback
    /// is `None`; use [`from_pair_with_base`] to exercise that path.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn from_pair(url: Option<String>, token: Option<String>) -> Self {
        let url_configured = url.as_deref().is_some_and(|s| !s.is_empty());
        Self::build(url, token, url_configured, None)
    }

    /// Like [`from_pair`] but with an explicit `AI_MEMORY_BASE_PATH` env
    /// fallback value, so the env-fallback branch is testable hermetically.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn from_pair_with_base(
        url: Option<String>,
        token: Option<String>,
        env_base: Option<String>,
    ) -> Self {
        let url_configured = url.as_deref().is_some_and(|s| !s.is_empty());
        Self::build(url, token, url_configured, env_base)
    }

    fn build(
        url: Option<String>,
        token: Option<String>,
        url_configured: bool,
        env_base: Option<String>,
    ) -> Self {
        let raw = url
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVER_URL.to_string());
        let (origin, url_path) = split_origin_and_path(&raw);
        // The URL path component wins over the env var: it is the more
        // explicit, request-target-level statement of where the server is.
        let base_path = if url_path.is_empty() {
            env_base.map_or_else(String::new, |s| normalize_prefix(&s))
        } else {
            normalize_prefix(&url_path)
        };
        Self {
            url: origin,
            base_path,
            auth_token: token.filter(|s| !s.is_empty()),
            url_configured,
        }
    }

    /// Join the resolved origin + base-path prefix + a root-absolute route
    /// `path` (e.g. `/admin/status`) into the full request URL. When no base
    /// path is configured this is byte-identical to the old
    /// `format!("{origin}{path}")`.
    pub(crate) fn build_url(&self, path: &str) -> String {
        format!("{}{}{path}", self.url, self.base_path)
    }

    /// Stable, credential-free identity for client-local state tied to this
    /// server. The normalized mount path is part of the identity; bearer
    /// material deliberately is not.
    pub(crate) fn identity(&self) -> String {
        let raw = format!("{}{}", self.url.trim_end_matches('/'), self.base_path);
        let Ok(mut parsed) = reqwest::Url::parse(&raw) else {
            return "<invalid-server-url>".to_owned();
        };
        let _ = parsed.set_username("");
        let _ = parsed.set_password(None);
        parsed.set_query(None);
        parsed.set_fragment(None);
        parsed.as_str().trim_end_matches('/').to_owned()
    }

    /// Apply auth header to a `reqwest::RequestBuilder` if a token is set.
    pub(crate) fn authenticate(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth_token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }
}

/// Split a server URL into `(origin, path)` where `origin` is
/// scheme+authority (`http://host:port`) and `path` is everything from the
/// first `/` after the authority (`/wiki`, possibly empty). Trailing slashes
/// are trimmed from both. A URL with no scheme separator is treated as an
/// opaque origin with no path.
pub(crate) fn split_origin_and_path(raw: &str) -> (String, String) {
    let trimmed = raw.trim_end_matches('/');
    if let Some(scheme_end) = trimmed.find("://") {
        let after_scheme = scheme_end + 3;
        if let Some(rel_slash) = trimmed[after_scheme..].find('/') {
            let split = after_scheme + rel_slash;
            return (
                trimmed[..split].to_string(),
                trimmed[split..].trim_end_matches('/').to_string(),
            );
        }
    }
    (trimmed.to_string(), String::new())
}

/// GET `<endpoint>{path}` with optional query params, deserialise JSON.
///
/// # Errors
/// Returns an error when the connection fails, the response is non-2xx,
/// or the body can't be deserialised into `T`.
pub async fn get_json<T: DeserializeOwned>(
    endpoint: &ServerEndpoint,
    path: &str,
    query: &[(&str, &str)],
) -> Result<T> {
    let client = reqwest::Client::new();
    let url = endpoint.build_url(path);
    let mut req = client.get(&url);
    if !query.is_empty() {
        req = req.query(query);
    }
    req = endpoint.authenticate(req);
    let resp = req
        .send()
        .await
        .map_err(|e| augment_connect_error(e, endpoint, &url))?;
    let resp = require_success(reqwest::Method::GET, resp).await?;
    resp.json::<T>()
        .await
        .with_context(|| format!("parsing JSON body from GET {url}"))
}

/// PATCH JSON body to `<endpoint>{path}`, deserialise JSON response.
///
/// # Errors
/// Same as [`get_json`].
pub async fn patch_json<B: Serialize, T: DeserializeOwned>(
    endpoint: &ServerEndpoint,
    path: &str,
    body: &B,
) -> Result<T> {
    let client = reqwest::Client::new();
    let url = endpoint.build_url(path);
    let req = endpoint.authenticate(client.patch(&url).json(body));
    let resp = req
        .send()
        .await
        .map_err(|e| augment_connect_error(e, endpoint, &url))?;
    let resp = require_success(reqwest::Method::PATCH, resp).await?;
    resp.json::<T>()
        .await
        .with_context(|| format!("parsing JSON body from PATCH {url}"))
}

/// POST JSON body to `<endpoint>{path}`, deserialise JSON response.
///
/// # Errors
/// Same as [`get_json`].
pub async fn post_json<B: Serialize, T: DeserializeOwned>(
    endpoint: &ServerEndpoint,
    path: &str,
    body: &B,
) -> Result<T> {
    post_json_with_query(endpoint, path, &[], body).await
}

/// POST JSON and require a successful response without decoding its body.
pub async fn post_json_no_content<B: Serialize>(
    endpoint: &ServerEndpoint,
    path: &str,
    body: &B,
) -> Result<()> {
    let client = reqwest::Client::new();
    let url = endpoint.build_url(path);
    let req = endpoint.authenticate(client.post(&url).json(body));
    let resp = req
        .send()
        .await
        .map_err(|e| augment_connect_error(e, endpoint, &url))?;
    require_success(reqwest::Method::POST, resp).await?;
    Ok(())
}

/// POST an empty body and require a successful response.
pub async fn post_empty(endpoint: &ServerEndpoint, path: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let url = endpoint.build_url(path);
    let req = endpoint.authenticate(client.post(&url));
    let resp = req
        .send()
        .await
        .map_err(|e| augment_connect_error(e, endpoint, &url))?;
    require_success(reqwest::Method::POST, resp).await?;
    Ok(())
}

/// POST JSON body to `<endpoint>{path}` with URL-encoded query params.
///
/// # Errors
/// Same as [`post_json`].
pub async fn post_json_with_query<B: Serialize, T: DeserializeOwned>(
    endpoint: &ServerEndpoint,
    path: &str,
    query: &[(&str, &str)],
    body: &B,
) -> Result<T> {
    let client = reqwest::Client::new();
    let url = build_url_with_query(endpoint, path, query)?;
    let req = client.post(&url);
    let req = endpoint.authenticate(req.json(body));
    let resp = req
        .send()
        .await
        .map_err(|e| augment_connect_error(e, endpoint, &url))?;
    let resp = require_success(reqwest::Method::POST, resp).await?;
    resp.json::<T>()
        .await
        .with_context(|| format!("parsing JSON body from POST {url}"))
}

fn build_url_with_query(
    endpoint: &ServerEndpoint,
    path: &str,
    query: &[(&str, &str)],
) -> Result<String> {
    let mut url = reqwest::Url::parse(&endpoint.build_url(path))?;
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query.iter().copied());
    }
    Ok(url.to_string())
}

/// Turn a low-level reqwest connect/timeout error into a friendlier
/// message that surfaces the resolved server URL. The common cases are
/// "Connection refused" and a connect that never completes: on Windows a
/// closed loopback port is routinely dropped (timed out) by the firewall
/// instead of being refused, and both mean the same operator problem —
/// the CLI defaulted to (or was pointed at) an endpoint where nothing
/// answered.
pub(crate) fn augment_connect_error(
    err: reqwest::Error,
    endpoint: &ServerEndpoint,
    url: &str,
) -> anyhow::Error {
    if error_chain_indicates_no_answer(&err) {
        let hint = if endpoint.url_configured {
            format!(
                "\nAI_MEMORY_SERVER_URL is set to {} but nothing answered \
                 (refused or timed out). Check the server is running, the \
                 port is reachable from this host, and (if remote) any \
                 firewall + bearer-token config matches.",
                endpoint.url
            )
        } else {
            format!(
                "\nAI_MEMORY_SERVER_URL is NOT set; the CLI defaulted to \
                 {} and nothing answered (refused or timed out). If your \
                 server lives on another machine (e.g. a homelab), `export \
                 AI_MEMORY_SERVER_URL=http://<server>:49374` and (if auth \
                 is on) `export AI_MEMORY_AUTH_TOKEN=<token>` before \
                 re-running.",
                endpoint.url
            )
        };
        anyhow::Error::new(err).context(format!("could not reach {url}.{hint}"))
    } else {
        anyhow::Error::new(err).context(format!("HTTP request to {url} failed"))
    }
}

/// Whether a request failed because nothing answered — a refused connection
/// or a timeout — rather than because the server answered with an error.
pub(crate) fn server_unreachable(err: &anyhow::Error) -> bool {
    let answered = err
        .chain()
        .any(|cause| cause.downcast_ref::<ServerResponseError>().is_some());
    !answered && err.chain().any(error_chain_indicates_no_answer)
}

/// Whether an error chain means "nothing answered at the transport
/// layer": reqwest classified it as a connect-phase or timeout failure,
/// or a `ConnectionRefused`/`TimedOut` io::Error — or its platform
/// wording — is buried in the source chain. reqwest wraps its errors
/// deeply, and matching only an explicit RST would hide the diagnosis
/// from exactly the hosts (windows-latest among them) whose firewalls
/// turn a refused loopback connect into a silent timeout.
fn error_chain_indicates_no_answer(err: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(req) = err.downcast_ref::<reqwest::Error>()
        && (req.is_connect() || req.is_timeout())
    {
        return true;
    }
    let mut src: Option<&dyn std::error::Error> = Some(err);
    while let Some(e) = src {
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::TimedOut
            )
        {
            return true;
        }
        let wording = e.to_string().to_ascii_lowercase();
        if wording.contains("connection refused") || wording.contains("timed out") {
            return true;
        }
        src = e.source();
    }
    false
}

/// Outcome of a server reachability probe: did anything answer on the
/// configured endpoint?
#[derive(Debug)]
pub(crate) enum ServerProbe {
    /// The server answered. Any HTTP status counts — a build older than
    /// `/healthz` still answers 404/405, and what the probe needs to know is
    /// that something is listening, not that it is healthy.
    Reachable,
    /// Nothing answered (connect error or timeout). Carries the underlying
    /// reqwest error so callers can reuse [`augment_connect_error`]'s
    /// diagnosis verbatim.
    Unreachable(reqwest::Error),
}

/// How long a reachability probe waits for an answer. Short on purpose: the
/// probe sits on the launch hot path, so an unresponsive host (a homelab down
/// for maintenance) must be classified in ~2s rather than the OS-level TCP
/// timeout a plain request would pay.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// GET `{origin}{base}/healthz` with a short timeout to decide whether a
/// managed launch may proceed without the server.
///
/// Any HTTP response — 200 or 404/405 from a pre-`/healthz` server — means
/// `Reachable`; only connect errors and timeouts mean `Unreachable`. The
/// probe never inspects the body, so it cannot be wrong about health vs
/// absence, only about presence.
pub(crate) async fn probe_server(endpoint: &ServerEndpoint) -> ServerProbe {
    probe_server_with_timeout(endpoint, PROBE_TIMEOUT).await
}

async fn probe_server_with_timeout(endpoint: &ServerEndpoint, timeout: Duration) -> ServerProbe {
    let client = reqwest::Client::new();
    let url = endpoint.build_url("/healthz");
    match client.get(&url).timeout(timeout).send().await {
        Ok(_) => ServerProbe::Reachable,
        Err(error) => ServerProbe::Unreachable(error),
    }
}

/// POST an empty body to `<endpoint>{path}`, streaming the response to `dest`.
///
/// Intended for routes whose response is binary (e.g. `POST /admin/backup`
/// returns an `application/gzip` tarball). On non-2xx the response body is
/// consumed and returned as an error string. Returns bytes written.
///
/// # Errors
/// Returns an error when the connection fails, the response is non-2xx,
/// or the body cannot be read or written.
pub async fn post_to_file(endpoint: &ServerEndpoint, path: &str, dest: &Path) -> Result<u64> {
    let client = reqwest::Client::new();
    let url = endpoint.build_url(path);
    let req = endpoint.authenticate(client.post(&url));
    let resp = req
        .send()
        .await
        .map_err(|e| augment_connect_error(e, endpoint, &url))?;
    let mut resp = require_success(reqwest::Method::POST, resp).await?;
    let file = private_output_file(dest)
        .with_context(|| format!("creating output file {}", dest.display()))?;
    let mut writer = BufWriter::new(file);
    let mut written = 0_u64;
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("reading response chunk from POST {url}"))?
    {
        writer
            .write_all(&chunk)
            .with_context(|| format!("writing response chunk to {}", dest.display()))?;
        written += chunk.len() as u64;
    }
    writer
        .flush()
        .with_context(|| format!("flushing {}", dest.display()))?;
    Ok(written)
}

/// Open a downloaded backup output file with private permissions before the
/// first response bytes are written. Existing user-selected output files keep
/// their existing permissions when overwritten.
fn private_output_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn only_an_unanswered_request_is_unreachable() {
        let refused =
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
                .context("HTTP request to http://127.0.0.1:1/admin/status failed");
        assert!(server_unreachable(&refused));
        let answered = server_response_error_for_test(
            reqwest::Method::GET,
            "/admin/status",
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "down for maintenance".to_owned(),
        );
        assert!(!server_unreachable(&answered));
        assert!(!server_unreachable(&anyhow::anyhow!(
            "malformed response body"
        )));
    }

    #[cfg(unix)]
    #[test]
    fn downloaded_backup_output_is_private_when_created() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("backup.tar.gz");
        private_output_file(&output).unwrap();
        assert_eq!(
            std::fs::metadata(output).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn response_errors_include_method_and_credential_free_path() {
        let url = reqwest::Url::parse(
            "https://username:password@example.test/workstream/runs?access_token=secret",
        )
        .expect("test URL parses");
        let error = server_response_error(
            reqwest::Method::POST,
            &url,
            reqwest::StatusCode::NOT_FOUND,
            "missing".to_owned(),
        );

        assert_eq!(
            error.to_string(),
            "POST /workstream/runs: server returned 404 Not Found: missing"
        );
        let structured = error
            .downcast_ref::<ServerResponseError>()
            .expect("response errors retain their structured type");
        assert_eq!(structured.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(structured.body(), "missing");
        assert!(!error.to_string().contains("username"));
        assert!(!error.to_string().contains("password"));
        assert!(!error.to_string().contains("access_token"));
        assert!(!error.to_string().contains("secret"));
    }

    // ----------------------------------------------------------------
    // ServerEndpoint::from_pair
    // ----------------------------------------------------------------

    #[test]
    fn from_pair_defaults_to_loopback_when_none() {
        let ep = ServerEndpoint::from_pair(None, None);
        assert_eq!(ep.url, "http://127.0.0.1:49374");
        assert!(ep.auth_token.is_none());
    }

    #[test]
    fn from_pair_defaults_to_loopback_when_empty() {
        let ep = ServerEndpoint::from_pair(Some(String::new()), None);
        assert_eq!(ep.url, "http://127.0.0.1:49374");
    }

    #[test]
    fn from_pair_strips_trailing_slash() {
        let ep = ServerEndpoint::from_pair(Some("http://10.0.0.1:8080/".to_string()), None);
        assert_eq!(ep.url, "http://10.0.0.1:8080");
    }

    #[test]
    fn from_pair_strips_multiple_trailing_slashes() {
        let ep = ServerEndpoint::from_pair(Some("http://10.0.0.1:8080///".to_string()), None);
        assert_eq!(ep.url, "http://10.0.0.1:8080");
    }

    #[test]
    fn from_pair_empty_token_treated_as_none() {
        let ep = ServerEndpoint::from_pair(None, Some(String::new()));
        assert!(ep.auth_token.is_none());
    }

    #[test]
    fn from_pair_non_empty_token_preserved() {
        let ep = ServerEndpoint::from_pair(None, Some("secret".to_string()));
        assert_eq!(ep.auth_token.as_deref(), Some("secret"));
    }

    #[test]
    fn client_state_identity_normalizes_mounts_and_never_contains_credentials() {
        let first = ServerEndpoint::from_pair(
            Some("http://alice:secret@MEMORY.example:49374/wiki/".to_owned()),
            None,
        );
        let second = ServerEndpoint::from_pair(
            Some("http://memory.example:49374/wiki".to_owned()),
            Some("bearer-secret".to_owned()),
        );

        assert_eq!(first.identity(), second.identity());
        assert!(!first.identity().contains("alice"));
        assert!(!first.identity().contains("secret"));
    }

    // ----------------------------------------------------------------
    // Base-path awareness — graduated from the Docker live exploration of
    // the CLI-client-under-AI_MEMORY_BASE_PATH bug. The server nests every
    // route under `--base-path` (`/wiki/admin/status`), but the thin client
    // baked root-absolute paths (`/admin/status`) → 404. Each test below
    // pins one hypothesis that the live run validated end-to-end.
    // ----------------------------------------------------------------

    /// H1 — no base path configured: the joined URL is byte-identical to the
    /// old `format!("{origin}{path}")`. The regression guard for the OFF-by-
    /// default promise.
    #[test]
    fn build_url_without_base_is_byte_identical() {
        let ep = ServerEndpoint::from_pair(Some("http://h:49374".to_string()), None);
        assert_eq!(ep.base_path, "");
        assert_eq!(ep.build_url("/admin/status"), "http://h:49374/admin/status");
    }

    /// H4/H9 — remote client: the base path comes from the **path component
    /// of the server URL**, which is split off the origin and normalised
    /// (trailing slash trimmed).
    #[test]
    fn server_url_path_becomes_base_path() {
        let ep = ServerEndpoint::from_pair(Some("http://h:49374/wiki".to_string()), None);
        assert_eq!(ep.url, "http://h:49374");
        assert_eq!(ep.base_path, "/wiki");
        assert_eq!(
            ep.build_url("/admin/status"),
            "http://h:49374/wiki/admin/status"
        );

        // Trailing slash on the URL path normalises away (H9).
        let ep = ServerEndpoint::from_pair(Some("http://h:49374/wiki/".to_string()), None);
        assert_eq!(ep.url, "http://h:49374");
        assert_eq!(ep.base_path, "/wiki");
    }

    /// H3 — in-pod client: no path on the server URL, base path falls back to
    /// the `AI_MEMORY_BASE_PATH` env value (passed explicitly here to stay
    /// hermetic).
    #[test]
    fn env_base_path_used_when_url_has_no_path() {
        let ep = ServerEndpoint::from_pair_with_base(None, None, Some("/wiki".to_string()));
        assert_eq!(ep.url, "http://127.0.0.1:49374");
        assert_eq!(ep.base_path, "/wiki");
        assert_eq!(
            ep.build_url("/admin/write-page"),
            "http://127.0.0.1:49374/wiki/admin/write-page"
        );
    }

    /// H8 — precedence: when BOTH a URL path and an env base are present, the
    /// URL path wins (it is the more explicit, request-target-level value).
    #[test]
    fn url_path_wins_over_env_base() {
        let ep = ServerEndpoint::from_pair_with_base(
            Some("http://h:49374/url-base".to_string()),
            None,
            Some("/env-base".to_string()),
        );
        assert_eq!(ep.base_path, "/url-base");
    }

    /// H11 — multi-segment base path is preserved verbatim.
    #[test]
    fn multi_segment_base_path_is_preserved() {
        let ep = ServerEndpoint::from_pair_with_base(None, None, Some("/a/b".to_string()));
        assert_eq!(ep.base_path, "/a/b");
        assert_eq!(
            ep.build_url("/admin/status"),
            "http://127.0.0.1:49374/a/b/admin/status"
        );
    }

    /// H10 — a traversal-y base (`/wiki/../etc`) is neutralised by
    /// `normalize_prefix` to empty (it rejects dot-segments), so the client
    /// falls back to the root rather than emitting `/wiki/../etc/...`. The
    /// client then 404s consistently instead of walking out of the prefix.
    #[test]
    fn traversal_base_path_is_neutralised_to_root() {
        let ep = ServerEndpoint::from_pair_with_base(None, None, Some("/wiki/../etc".to_string()));
        assert_eq!(ep.base_path, "");
        let joined = ep.build_url("/admin/status");
        assert!(
            !joined.contains("/../") && !joined.contains("/etc/admin"),
            "traversal must not leak into the request URL; got {joined}"
        );
        assert_eq!(joined, "http://127.0.0.1:49374/admin/status");
    }

    /// A bare base value without a leading slash is normalised to `/<core>`,
    /// matching the server's own `normalize_prefix` so the two agree.
    #[test]
    fn bare_env_base_gets_leading_slash() {
        let ep = ServerEndpoint::from_pair_with_base(None, None, Some("wiki".to_string()));
        assert_eq!(ep.base_path, "/wiki");
    }

    /// `/` and empty both mean "root" — no prefix added.
    #[test]
    fn root_like_base_values_mean_no_prefix() {
        for raw in ["", "/", "//", "  /  "] {
            let ep = ServerEndpoint::from_pair_with_base(None, None, Some(raw.to_string()));
            assert_eq!(ep.base_path, "", "{raw:?} should normalise to no prefix");
            assert_eq!(
                ep.build_url("/admin/status"),
                "http://127.0.0.1:49374/admin/status"
            );
        }
    }

    #[test]
    fn build_url_with_query_url_encodes_values() {
        let ep = ServerEndpoint::from_pair(Some("http://h:49374/wiki".to_string()), None);
        let url = super::build_url_with_query(
            &ep,
            "/admin/pending-writes/id/approve",
            &[("workspace", "default workspace"), ("project", "a/b & c")],
        )
        .unwrap();
        assert_eq!(
            url,
            "http://h:49374/wiki/admin/pending-writes/id/approve?workspace=default+workspace&project=a%2Fb+%26+c"
        );
    }

    // ----------------------------------------------------------------
    // split_origin_and_path
    // ----------------------------------------------------------------

    #[test]
    fn split_origin_and_path_cases() {
        assert_eq!(
            split_origin_and_path("http://h:49374"),
            ("http://h:49374".to_string(), String::new())
        );
        assert_eq!(
            split_origin_and_path("http://h:49374/"),
            ("http://h:49374".to_string(), String::new())
        );
        assert_eq!(
            split_origin_and_path("http://h:49374/wiki"),
            ("http://h:49374".to_string(), "/wiki".to_string())
        );
        assert_eq!(
            split_origin_and_path("http://h:49374/wiki/"),
            ("http://h:49374".to_string(), "/wiki".to_string())
        );
        assert_eq!(
            split_origin_and_path("https://h:49374/a/b"),
            ("https://h:49374".to_string(), "/a/b".to_string())
        );
        // No scheme separator → opaque origin, no path split.
        assert_eq!(
            split_origin_and_path("127.0.0.1:49374"),
            ("127.0.0.1:49374".to_string(), String::new())
        );
    }

    // ----------------------------------------------------------------
    // ServerEndpoint::authenticate
    // ----------------------------------------------------------------

    #[test]
    fn authenticate_no_token_leaves_request_unchanged() {
        let ep = ServerEndpoint::from_pair(None, None);
        let client = reqwest::Client::new();
        // Build a request, authenticate it, then build to inspect.
        let req = ep
            .authenticate(client.get("http://localhost"))
            .build()
            .unwrap();
        // No Authorization header should be present.
        assert!(
            req.headers().get("authorization").is_none(),
            "no Authorization header expected"
        );
    }

    #[test]
    fn authenticate_with_token_sets_bearer_header() {
        let ep = ServerEndpoint::from_pair(None, Some("tok123".to_string()));
        let client = reqwest::Client::new();
        let req = ep
            .authenticate(client.get("http://localhost"))
            .build()
            .unwrap();
        let auth = req
            .headers()
            .get("authorization")
            .expect("Authorization header must be set")
            .to_str()
            .unwrap();
        assert_eq!(auth, "Bearer tok123");
    }

    // ----------------------------------------------------------------
    // Server reachability probe
    // ----------------------------------------------------------------

    /// Bind a loopback listener, capture its address, and release it: the
    /// next connection to that address is refused. Rebinding races exist on
    /// loopback but are vanishingly rare under a test load.
    async fn closed_port_endpoint() -> ServerEndpoint {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        ServerEndpoint::from_pair(Some(format!("http://{address}")), None)
    }

    #[tokio::test]
    async fn probe_reports_a_closed_port_as_unreachable_with_the_diagnosis() {
        let endpoint = closed_port_endpoint().await;
        let ServerProbe::Unreachable(error) = probe_server(&endpoint).await else {
            panic!("a closed port must be Unreachable");
        };
        let url = endpoint.build_url("/workstream/runs");
        let augmented = augment_connect_error(error, &endpoint, &url);
        let message = format!("{augmented:#}");
        assert!(
            message.contains("could not reach"),
            "the probe error must feed augment_connect_error: {message}"
        );
        assert!(
            message.contains(&endpoint.url),
            "the diagnosis names the resolved server URL: {message}"
        );
    }

    /// A connect that is accepted but never answered ends as a reqwest
    /// timeout — the variant windows-latest produces when its firewall
    /// drops a loopback connect instead of refusing it. The operator
    /// problem ("nothing at this URL") is identical to a refusal, so the
    /// diagnosis must be too.
    #[tokio::test]
    async fn probe_timeout_error_carries_the_same_reachability_diagnosis() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // Accept connections and hold them open without answering: the
            // client's only way out is its own timeout.
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        let endpoint = ServerEndpoint::from_pair(Some(format!("http://{address}")), None);

        let ServerProbe::Unreachable(error) =
            probe_server_with_timeout(&endpoint, Duration::from_millis(150)).await
        else {
            panic!("an accept-and-hold socket must end Unreachable via the probe timeout");
        };
        assert!(
            error.is_timeout(),
            "accept-and-hold must produce the timeout variant, got: {error}"
        );
        let url = endpoint.build_url("/workstream/runs");
        let augmented = augment_connect_error(error, &endpoint, &url);
        let message = format!("{augmented:#}");
        assert!(
            message.contains("could not reach"),
            "the timeout variant must keep the reachability diagnosis: {message}"
        );
        assert!(
            message.contains(&endpoint.url),
            "the diagnosis names the resolved server URL: {message}"
        );
    }

    /// The no-answer detection must recognize every platform's wording of
    /// "nothing answered": the typed io kinds, and the free-text variants
    /// hyper buries below reqwest on each OS. windows-latest says
    /// "operation timed out (os error 10060)" where Linux says
    /// "Connection refused (os error 111)".
    #[test]
    fn no_answer_detection_covers_refused_and_timeout_wording() {
        #[derive(Debug)]
        struct Wrapped(&'static str);
        impl std::fmt::Display for Wrapped {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Wrapped {}

        assert!(error_chain_indicates_no_answer(&std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        )));
        assert!(error_chain_indicates_no_answer(&std::io::Error::from(
            std::io::ErrorKind::TimedOut
        )));
        assert!(error_chain_indicates_no_answer(&Wrapped(
            "error sending request for url (http://127.0.0.1:49374/healthz): operation timed \
             out (os error 10060)"
        )));
        assert!(error_chain_indicates_no_answer(&Wrapped(
            "error sending request for url (http://127.0.0.1:49374/healthz): Connection refused \
             (os error 111)"
        )));
        // Unrelated transport failures keep the generic HTTP-failure
        // message: a decode error answered, it did not go unanswered.
        assert!(!error_chain_indicates_no_answer(&Wrapped(
            "error decoding response body"
        )));
    }

    /// A server that answers 404 for `/healthz` (older than the route) is
    /// still reachable: presence, not health, is what the probe decides.
    #[tokio::test]
    async fn probe_reports_any_http_answer_as_reachable() {
        for status in [
            axum::http::StatusCode::OK,
            axum::http::StatusCode::NOT_FOUND,
            axum::http::StatusCode::METHOD_NOT_ALLOWED,
        ] {
            let app =
                axum::Router::new().fallback(axum::routing::get(move || async move { status }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let endpoint = ServerEndpoint::from_pair(Some(format!("http://{address}")), None);

            assert!(matches!(
                probe_server(&endpoint).await,
                ServerProbe::Reachable
            ));

            server.abort();
        }
    }

    /// A listener that accepts but never answers must be given up on within
    /// the probe timeout, not a TCP or default-client timeout.
    #[tokio::test]
    async fn probe_gives_up_within_its_own_timeout() {
        async fn accept_and_hold() -> std::net::SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            tokio::spawn(async move {
                // Accept connections, hold them open, and answer nothing —
                // the streams must outlive the accept so the client's only
                // way out is its own timeout.
                let mut held = Vec::new();
                while let Ok((stream, _)) = listener.accept().await {
                    held.push(stream);
                }
            });
            address
        }
        let address = accept_and_hold().await;
        let endpoint = ServerEndpoint::from_pair(Some(format!("http://{address}")), None);

        let started = Instant::now();
        assert!(matches!(
            probe_server_with_timeout(&endpoint, Duration::from_millis(150)).await,
            ServerProbe::Unreachable(_)
        ));
        let elapsed = started.elapsed();
        assert!(elapsed >= Duration::from_millis(150), "gave up too early");
        assert!(
            elapsed < Duration::from_secs(2),
            "the probe timeout, not a transport default, bounds the wait ({elapsed:?})"
        );
    }

    /// The production probe stays bounded by design: a homelab down for
    /// maintenance must be classified in ~2s, never by an OS TCP timeout.
    #[test]
    fn probe_timeout_is_bounded_to_about_two_seconds() {
        assert!(PROBE_TIMEOUT <= Duration::from_secs(2));
        assert!(!PROBE_TIMEOUT.is_zero());
    }

    #[tokio::test]
    async fn from_config_resolving_auth_uses_stored_oidc_for_authorization_header() {
        let tmp = tempfile::tempdir().unwrap();
        let mut config = Config {
            data_dir: tmp.path().to_path_buf(),
            ..Config::default()
        };
        config.auth.bearer_token = None;
        ai_memory_llm::OidcToken {
            access: secrecy::SecretString::from("oidc-access".to_string()),
            refresh: secrecy::SecretString::from("refresh-token".to_string()),
            expires_at_ms: u64::MAX,
            extra: ai_memory_llm::OidcExtras {
                issuer: "https://issuer.example.com/realms/team".to_string(),
                client_id: "ai-memory-cli".to_string(),
                token_endpoint: "https://issuer.example.com/token".to_string(),
            },
        }
        .save(&config.oidc_device_token_path())
        .expect("save test OIDC token");

        let ep = ServerEndpoint::from_config_resolving_auth(&config).await;
        let client = reqwest::Client::new();
        let req = ep
            .authenticate(client.get("http://localhost"))
            .build()
            .unwrap();
        let auth = req
            .headers()
            .get("authorization")
            .expect("Authorization header must be set")
            .to_str()
            .unwrap();

        assert_eq!(auth, "Bearer oidc-access");
    }
}
