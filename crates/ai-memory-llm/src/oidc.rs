//! Generic OIDC Device Authorization Grant provider.
//!
//! Lets a developer sign the ai-memory CLI in against any OIDC issuer (e.g.
//! Keycloak) with the device-authorization grant, so headless lifecycle hooks
//! can POST to the server's `/hook` and `/handoff` routes carrying a
//! **per-developer** JWT instead of a shared static token. The stored token is
//! refreshed on demand from the hook fast-path.
//!
//! Storage uses the shared [`crate::stored_token`] core: the same shared
//! `auth.json` file as [`crate::openai_oauth`], under the `"oidc"` key, with
//! `type: "oauth"` so the shared loader accepts it. The server side is
//! unchanged — its auth layer already validates a Keycloak JWT on the hook
//! routes (requiring the `mcp:read` realm role).

use std::path::Path;

use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};

use crate::auth_file::now_ms;
use crate::error::{LlmError, LlmResult};
use crate::response::{provider_error_body, response_json_limited};
use crate::stored_token::{StoredOAuthToken, refresh_grant};

/// Default device-flow scopes: `openid` (ID token), `profile` (so the access
/// token carries `preferred_username` → `X-Memory-Actor-User` on the server),
/// and `offline_access` (refresh token). The `mcp:read` authorization the
/// server requires is a **realm role** on the user, not a scope.
pub const OIDC_DEFAULT_SCOPE: &str = "openid profile offline_access";

/// OIDC-specific fields persisted next to the shared token triple.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OidcExtras {
    /// OIDC issuer this token was minted by (kept for `status` display).
    pub issuer: String,
    /// Public client id used for the device + refresh grants.
    pub client_id: String,
    /// Token endpoint resolved at login, so refresh needs no re-discovery.
    pub token_endpoint: String,
}

/// Stored OIDC device-grant token.
pub type OidcToken = StoredOAuthToken<OidcExtras>;

impl OidcToken {
    /// Build a token from a token-endpoint response. `previous_refresh` is used
    /// when a refresh response omits a new `refresh_token` (the old one stays
    /// valid), mirroring the OpenAI OAuth provider.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when no refresh token is available — the
    /// device flow must request the `offline_access` scope.
    pub fn from_token_response(
        resp: &OidcTokenResponse,
        issuer: impl Into<String>,
        client_id: impl Into<String>,
        token_endpoint: impl Into<String>,
        previous_refresh: Option<&str>,
    ) -> LlmResult<Self> {
        let refresh = resp
            .refresh_token
            .clone()
            .or_else(|| previous_refresh.map(str::to_string))
            .ok_or_else(|| {
                LlmError::Auth(
                    "OIDC token response carried no refresh_token; request the \
                     `offline_access` scope and enable it on the Keycloak client"
                        .to_string(),
                )
            })?;
        Ok(Self {
            access: SecretString::from(resp.access_token.clone()),
            refresh: SecretString::from(refresh),
            expires_at_ms: now_ms()
                .saturating_add(resp.expires_in.unwrap_or(300).saturating_mul(1000)),
            extra: OidcExtras {
                issuer: issuer.into(),
                client_id: client_id.into(),
                token_endpoint: token_endpoint.into(),
            },
        })
    }

    /// Load the OIDC token from the shared `auth.json` file.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file exists but cannot be read/parsed.
    pub fn load(path: &Path) -> LlmResult<Option<Self>> {
        Self::load_key(path, "oidc")
    }

    /// Save the token into the shared `auth.json` file, preserving other keys.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file cannot be written.
    pub fn save(&self, path: &Path) -> LlmResult<()> {
        self.save_key(path, "oidc")
    }

    /// Remove the OIDC entry from the shared `auth.json` file.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file cannot be updated.
    pub fn remove(path: &Path) -> LlmResult<()> {
        Self::remove_key(path, "oidc")
    }
}

/// The two endpoints we need from `<issuer>/.well-known/openid-configuration`.
#[derive(Debug, Deserialize)]
pub struct OidcDiscovery {
    /// Device-authorization endpoint (RFC 8628).
    pub device_authorization_endpoint: String,
    /// Token endpoint (device-code + refresh grants).
    pub token_endpoint: String,
}

/// Device-authorization response (RFC 8628 §3.2).
#[derive(Debug, Deserialize)]
pub struct DeviceAuthorizationResponse {
    /// Opaque device verification code, polled at the token endpoint.
    pub device_code: String,
    /// Short code the user types at the verification URI.
    pub user_code: String,
    /// URI the user opens to authorize the device.
    pub verification_uri: String,
    /// Verification URI with the `user_code` pre-filled, when provided.
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Minimum seconds to wait between polls (default 5).
    #[serde(default)]
    pub interval: Option<u64>,
    /// Lifetime of the device code in seconds.
    #[serde(default)]
    pub expires_in: Option<u64>,
}

/// Token-endpoint success response.
#[derive(Debug, Deserialize)]
pub struct OidcTokenResponse {
    /// Bearer access token for the ai-memory server.
    pub access_token: String,
    /// Refresh token (present when `offline_access` was granted).
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Access-token lifetime in seconds.
    #[serde(default)]
    pub expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenErrorResponse {
    #[serde(default)]
    error: Option<String>,
}

/// Outcome of one device-token poll (RFC 8628 §3.5).
#[derive(Debug)]
pub enum PollOutcome {
    /// Keep polling at the current interval.
    Pending,
    /// Back off: increase the interval by 5s, per the spec.
    SlowDown,
    /// Authorization complete.
    Token(Box<OidcTokenResponse>),
    /// The user denied the request.
    Denied,
    /// The device code expired before authorization.
    Expired,
    /// Any other terminal `error` code from the token endpoint.
    Other(String),
}

/// Resolve the device + token endpoints from the issuer's discovery document.
///
/// # Errors
/// Returns [`LlmError`] on transport failure or a non-success status.
pub async fn discover(client: &reqwest::Client, issuer: &str) -> LlmResult<OidcDiscovery> {
    let url = format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    );
    let resp = client.get(&url).send().await.map_err(LlmError::from)?;
    let status = resp.status();
    if !status.is_success() {
        let body = provider_error_body(resp).await;
        return Err(LlmError::Auth(format!(
            "OIDC discovery failed ({status}) for {url}: {body}"
        )));
    }
    response_json_limited::<OidcDiscovery>(resp).await
}

/// Start the device-authorization grant for a public client.
///
/// # Errors
/// Returns [`LlmError`] on transport failure or a non-success status.
pub async fn request_device_code(
    client: &reqwest::Client,
    discovery: &OidcDiscovery,
    client_id: &str,
    scope: &str,
) -> LlmResult<DeviceAuthorizationResponse> {
    let resp = client
        .post(&discovery.device_authorization_endpoint)
        .form(&[("client_id", client_id), ("scope", scope)])
        .send()
        .await
        .map_err(LlmError::from)?;
    let status = resp.status();
    if !status.is_success() {
        let body = provider_error_body(resp).await;
        return Err(LlmError::Auth(format!(
            "device authorization request failed ({status}): {body}"
        )));
    }
    response_json_limited::<DeviceAuthorizationResponse>(resp).await
}

/// Poll the token endpoint once with a device code.
///
/// # Errors
/// Returns [`LlmError`] only on transport failure; OAuth `error` codes
/// (`authorization_pending`, `slow_down`, …) are returned as [`PollOutcome`].
pub async fn poll_token_once(
    client: &reqwest::Client,
    discovery: &OidcDiscovery,
    client_id: &str,
    device_code: &str,
) -> LlmResult<PollOutcome> {
    let resp = client
        .post(&discovery.token_endpoint)
        .form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            ("client_id", client_id),
        ])
        .send()
        .await
        .map_err(LlmError::from)?;
    if resp.status().is_success() {
        let token = response_json_limited::<OidcTokenResponse>(resp).await?;
        return Ok(PollOutcome::Token(Box::new(token)));
    }
    let body = provider_error_body(resp).await;
    let code = serde_json::from_str::<TokenErrorResponse>(&body)
        .ok()
        .and_then(|e| e.error);
    Ok(match code.as_deref() {
        Some("authorization_pending") => PollOutcome::Pending,
        Some("slow_down") => PollOutcome::SlowDown,
        Some("access_denied") => PollOutcome::Denied,
        Some("expired_token") => PollOutcome::Expired,
        Some(other) => PollOutcome::Other(other.to_string()),
        None => PollOutcome::Other(body),
    })
}

/// Exchange the refresh token for a fresh access token.
///
/// # Errors
/// Returns [`LlmError::Auth`] when the refresh grant is rejected (the caller
/// should fall back to the stale token or prompt a re-login).
pub async fn refresh_access_token(
    client: &reqwest::Client,
    token: &OidcToken,
) -> LlmResult<OidcToken> {
    let token_response = refresh_grant::<OidcTokenResponse>(
        client,
        &token.extra.token_endpoint,
        token.refresh.expose_secret(),
        &token.extra.client_id,
        "OIDC refresh failed",
        "Run `ai-memory auth login oidc-device` again.",
    )
    .await?;
    OidcToken::from_token_response(
        &token_response,
        token.extra.issuer.clone(),
        token.extra.client_id.clone(),
        token.extra.token_endpoint.clone(),
        Some(token.refresh.expose_secret()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OidcToken {
        OidcToken {
            access: SecretString::from("access-x".to_string()),
            refresh: SecretString::from("refresh-y".to_string()),
            expires_at_ms: now_ms() + 3_600_000,
            extra: OidcExtras {
                issuer: "https://kc.example.com/realms/ai-memory".to_string(),
                client_id: "ai-memory-dev".to_string(),
                token_endpoint:
                    "https://kc.example.com/realms/ai-memory/protocol/openid-connect/token"
                        .to_string(),
            },
        }
    }

    #[test]
    fn token_round_trips_through_auth_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let token = sample();
        token.save(&path).unwrap();
        let loaded = OidcToken::load(&path).unwrap().unwrap();
        assert_eq!(loaded.access.expose_secret(), "access-x");
        assert_eq!(loaded.refresh.expose_secret(), "refresh-y");
        assert_eq!(loaded.extra.client_id, "ai-memory-dev");
        assert_eq!(
            loaded.extra.token_endpoint,
            "https://kc.example.com/realms/ai-memory/protocol/openid-connect/token"
        );
    }

    #[test]
    fn save_preserves_sibling_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "openai": { "type": "oauth", "access": "a", "refresh": "b", "expires": 1 }
            }))
            .unwrap(),
        )
        .unwrap();
        sample().save(&path).unwrap();
        let value =
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert!(value.get("openai").is_some(), "sibling entry preserved");
        assert_eq!(value["oidc"]["client_id"], "ai-memory-dev");
        assert_eq!(value["oidc"]["type"], "oauth");
    }

    #[test]
    fn remove_clears_only_the_oidc_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        sample().save(&path).unwrap();
        OidcToken::remove(&path).unwrap();
        assert!(OidcToken::load(&path).unwrap().is_none());
    }

    #[test]
    fn needs_refresh_true_when_expired() {
        let mut token = sample();
        token.expires_at_ms = now_ms();
        assert!(token.needs_refresh());
    }

    #[test]
    fn from_response_requires_a_refresh_token() {
        let resp = OidcTokenResponse {
            access_token: "a".into(),
            refresh_token: None,
            expires_in: Some(300),
        };
        let err = OidcToken::from_token_response(&resp, "iss", "cid", "tok", None);
        assert!(err.is_err(), "missing refresh token must error");
        // ...unless a previous refresh token is carried forward.
        let ok = OidcToken::from_token_response(&resp, "iss", "cid", "tok", Some("old-refresh"));
        assert_eq!(ok.unwrap().refresh.expose_secret(), "old-refresh");
    }
}
