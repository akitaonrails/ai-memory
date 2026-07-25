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
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::config::{Config, DEFAULT_SERVER_URL};
use ai_memory_web::normalize_prefix;

/// Non-success response returned by the configured ai-memory server.
#[derive(Debug)]
pub(crate) struct ServerResponseError {
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
        write!(formatter, "server returned {}: {}", self.status, self.body)
    }
}

impl std::error::Error for ServerResponseError {}

fn server_response_error(status: reqwest::StatusCode, body: String) -> anyhow::Error {
    ServerResponseError { status, body }.into()
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
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(server_response_error(status, body));
    }
    resp.json::<T>()
        .await
        .with_context(|| format!("parsing JSON body from GET {url}"))
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
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(server_response_error(status, body));
    }
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
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(server_response_error(status, body));
    }
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
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(server_response_error(status, body));
    }
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
/// message that surfaces the resolved server URL. The common case is
/// "Connection refused" — typically because the CLI defaulted to
/// loopback on a host that has no local server running.
fn augment_connect_error(
    err: reqwest::Error,
    endpoint: &ServerEndpoint,
    url: &str,
) -> anyhow::Error {
    // Walk the source chain to see if there's a Connection-refused
    // io::Error buried somewhere. reqwest wraps its errors deeply.
    let chain_contains_refused = {
        let mut src: Option<&dyn std::error::Error> = Some(&err);
        let mut found = false;
        while let Some(e) = src {
            if e.to_string().contains("Connection refused")
                || e.to_string().contains("connection refused")
            {
                found = true;
                break;
            }
            src = e.source();
        }
        found
    };

    if chain_contains_refused {
        let hint = if endpoint.url_configured {
            format!(
                "\nAI_MEMORY_SERVER_URL is set to {} but nothing answered. \
                 Check the server is running, the port is reachable from \
                 this host, and (if remote) any firewall + bearer-token \
                 config matches.",
                endpoint.url
            )
        } else {
            format!(
                "\nAI_MEMORY_SERVER_URL is NOT set; the CLI defaulted to \
                 {} and nothing answered. If your server lives on another \
                 machine (e.g. a homelab), `export AI_MEMORY_SERVER_URL=\
                 http://<server>:49374` and (if auth is on) \
                 `export AI_MEMORY_AUTH_TOKEN=<token>` before re-running.",
                endpoint.url
            )
        };
        anyhow::Error::new(err).context(format!("could not reach {url}.{hint}"))
    } else {
        anyhow::Error::new(err).context(format!("HTTP request to {url} failed"))
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
    let mut resp = req
        .send()
        .await
        .map_err(|e| augment_connect_error(e, endpoint, &url))?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(server_response_error(status, body));
    }
    let file = std::fs::File::create(dest)
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

#[cfg(test)]
mod tests {
    use super::*;

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
