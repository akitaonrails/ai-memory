//! Shared core for OAuth tokens persisted in the shared `auth.json` file.
//!
//! Both token-backed providers ([`crate::openai_oauth`] and [`crate::oidc`])
//! store the same access/refresh/expiry triple under their own top-level key,
//! differing only in provider-specific extras. This module holds the shared
//! triple, the storage round-trip, and the refresh-grant POST flow so each
//! provider only defines its extras and its response mapping.

use std::path::Path;
use std::time::Duration;

use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::auth_file::{load_entry, now_ms, save_entry};
use crate::error::{LlmError, LlmResult};
use crate::response::{provider_error_body, provider_error_in, response_json_limited};

/// A credential-bearing HTTP endpoint validated as TLS-protected, except for
/// explicit loopback development servers. Keeping the parsed URL behind this
/// type prevents a new grant caller from reaching the request sink with an
/// unchecked string.
pub(crate) struct CredentialEndpoint(reqwest::Url);

impl CredentialEndpoint {
    pub(crate) fn as_url(&self) -> &reqwest::Url {
        &self.0
    }
}

pub(crate) fn credential_endpoint(context: &str, url: &str) -> LlmResult<CredentialEndpoint> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| LlmError::Auth(format!("{context} {url:?} is not a valid URL: {e}")))?;
    let loopback = parsed.host_str().is_some_and(|host| {
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    match parsed.scheme() {
        "https" => Ok(CredentialEndpoint(parsed)),
        "http" if loopback => Ok(CredentialEndpoint(parsed)),
        _ => Err(LlmError::Auth(format!(
            "{context} {url:?} must use https (plain http is accepted only on \
             localhost, 127.0.0.1 or ::1)"
        ))),
    }
}

/// Refresh once the access token is within this margin of expiry.
const REFRESH_MARGIN_MS: u64 = 60_000;

/// Access/refresh/expiry triple shared by every stored OAuth token, with
/// provider-specific claims in `extra` (persisted next to the triple under
/// the same `auth.json` entry).
#[derive(Clone)]
pub struct StoredOAuthToken<E> {
    /// Access token sent as the bearer token.
    pub access: SecretString,
    /// Refresh token used to mint a new access token.
    pub refresh: SecretString,
    /// Expiry in milliseconds since the Unix epoch.
    pub expires_at_ms: u64,
    /// Provider-specific fields persisted next to the core triple.
    pub extra: E,
}

impl<E: std::fmt::Debug> std::fmt::Debug for StoredOAuthToken<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredOAuthToken")
            .field("access", &"<redacted>")
            .field("refresh", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("extra", &self.extra)
            .finish()
    }
}

/// On-disk entry shape: the core triple plus the provider extras, flattened
/// so the extra keys sit at the top level of the entry object — the persisted
/// format predates this split and must not change.
#[derive(Debug, Serialize, Deserialize)]
struct OAuthEntry<E> {
    #[serde(rename = "type")]
    kind: String,
    access: String,
    refresh: String,
    expires: u64,
    #[serde(flatten)]
    extra: E,
}

impl<E> StoredOAuthToken<E> {
    /// True when the access token is expired or within the refresh margin.
    #[must_use]
    pub fn needs_refresh(&self) -> bool {
        now_ms().saturating_add(REFRESH_MARGIN_MS) >= self.expires_at_ms
    }

    /// Load the token stored under `key` in the shared `auth.json` file.
    pub(crate) fn load_key(path: &Path, key: &str) -> LlmResult<Option<Self>>
    where
        E: DeserializeOwned,
    {
        let Some(entry) = load_entry::<OAuthEntry<E>>(path, key)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            access: SecretString::from(entry.access),
            refresh: SecretString::from(entry.refresh),
            expires_at_ms: entry.expires,
            extra: entry.extra,
        }))
    }

    /// Save the token under `key`, preserving other keys in the file.
    pub(crate) fn save_key(&self, path: &Path, key: &str) -> LlmResult<()>
    where
        E: Serialize,
    {
        let entry = OAuthEntry {
            kind: "oauth".into(),
            access: self.access.expose_secret().to_string(),
            refresh: self.refresh.expose_secret().to_string(),
            expires: self.expires_at_ms,
            extra: &self.extra,
        };
        save_entry(path, key, Some(entry))
    }

    /// Remove the entry stored under `key` from the shared `auth.json` file.
    pub(crate) fn remove_key(path: &Path, key: &str) -> LlmResult<()>
    where
        E: Serialize,
    {
        save_entry::<OAuthEntry<E>>(path, key, None)
    }
}

/// POST a `grant_type=refresh_token` form to `token_endpoint` and parse the
/// success body. Shared by every stored-token provider; each maps the parsed
/// response onto its own token shape afterwards.
///
/// # Errors
/// Returns [`LlmError::Auth`] when the grant is rejected; `failure_label` and
/// `relogin_hint` keep the message provider-specific (e.g. "OIDC refresh
/// failed" and the matching re-login hint).
pub(crate) async fn refresh_grant<T>(
    client: &reqwest::Client,
    token_endpoint: &CredentialEndpoint,
    refresh_token: &str,
    client_id: &str,
    failure_label: &str,
    relogin_hint: &str,
) -> LlmResult<T>
where
    T: DeserializeOwned,
{
    let resp = client
        .post(token_endpoint.as_url().clone())
        // Bounded here because callers hand us their shared client, whose
        // global timeout was removed in favour of per-request timeouts —
        // a token exchange is quick, so the default ceiling is plenty.
        .timeout(Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await
        .map_err(LlmError::from)?;
    let status = resp.status();
    if !status.is_success() {
        // A rate-limited or overloaded token endpoint has not rejected the
        // grant, so it must stay transient and must not print a re-login hint
        // for a refresh token that is still valid.
        if LlmError::status_is_transient(status.as_u16()) {
            return Err(provider_error_in(failure_label, resp).await);
        }
        let body = provider_error_body(resp).await;
        return Err(LlmError::Auth(format!(
            "{failure_label} ({status}): {body}. {relogin_hint}"
        )));
    }
    response_json_limited::<T>(resp).await
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct TestResponse {
        access_token: String,
    }

    /// Serve a single fixed HTTP response on a loopback port, returning the
    /// URL to POST to (mirrors `auth_bearer.rs`'s refresh-flow tests).
    async fn serve_once(status: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}/token")
    }

    #[tokio::test]
    async fn refresh_grant_parses_success_body() {
        let endpoint = serve_once("200 OK", r#"{"access_token":"fresh-access"}"#).await;
        let endpoint = credential_endpoint("test token endpoint", &endpoint).unwrap();
        let client = reqwest::Client::new();

        let parsed = refresh_grant::<TestResponse>(
            &client,
            &endpoint,
            "refresh-x",
            "client-y",
            "test refresh failed",
            "Run `test login` again.",
        )
        .await
        .unwrap();

        assert_eq!(parsed.access_token, "fresh-access");
    }

    #[tokio::test]
    async fn refresh_grant_treats_a_rate_limited_endpoint_as_transient() {
        for (wire, code) in [
            ("429 Too Many Requests", 429u16),
            ("503 Service Unavailable", 503),
        ] {
            let endpoint = serve_once(wire, r#"{"error":"slow_down"}"#).await;
            let endpoint = credential_endpoint("test token endpoint", &endpoint).unwrap();
            let client = reqwest::Client::new();

            let err = refresh_grant::<TestResponse>(
                &client,
                &endpoint,
                "refresh-x",
                "client-y",
                "test refresh failed",
                "Run `test login` again.",
            )
            .await
            .expect_err("a throttled token endpoint must fail");

            assert!(
                err.is_transient(),
                "{wire} has not rejected the grant: {err:?}"
            );
            assert_eq!(err.http_status(), Some(code), "{wire}: {err:?}");
            assert!(
                err.to_string().contains("test refresh failed"),
                "{wire} must name the failing exchange: {err}"
            );
            assert!(
                !err.to_string().contains("Run `test login` again."),
                "{wire} must not tell the operator to re-login: {err}"
            );
        }
    }

    #[tokio::test]
    async fn refresh_grant_surfaces_status_body_and_relogin_hint() {
        let endpoint = serve_once("401 Unauthorized", r#"{"error":"invalid_grant"}"#).await;
        let endpoint = credential_endpoint("test token endpoint", &endpoint).unwrap();
        let client = reqwest::Client::new();

        let err = refresh_grant::<TestResponse>(
            &client,
            &endpoint,
            "refresh-x",
            "client-y",
            "test refresh failed",
            "Run `test login` again.",
        )
        .await
        .expect_err("rejected grant must fail");

        match err {
            LlmError::Auth(msg) => {
                assert!(
                    msg.contains("test refresh failed (401 Unauthorized)"),
                    "label+status: {msg}"
                );
                assert!(msg.contains("invalid_grant"), "upstream body: {msg}");
                assert!(msg.contains("Run `test login` again."), "hint: {msg}");
            }
            other => panic!("expected auth error, got {other:?}"),
        }
    }
}
