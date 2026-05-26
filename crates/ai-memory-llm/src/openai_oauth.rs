//! OpenAI OAuth 2.0 PKCE provider.
//!
//! Authenticates via an OAuth access token (obtained through `ai-memory
//! auth login`) instead of a static API key. The token is refreshed
//! on-demand when `expires_at - 60s` is in the past; the refreshed token
//! is immediately flushed to disk so the next process startup doesn't need
//! another browser round-trip.
//!
//! Wire format is identical to `OpenAiProvider` — the same
//! `Authorization: Bearer <token>` header, same `api.openai.com` endpoint.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::error::{LlmError, LlmResult};
use crate::openai::{OpenAiProvider, RequestDialect};
use crate::provider::LlmProvider;
use crate::types::{ChatRequest, ChatResponse};

/// OpenAI OAuth 2.0 token endpoint.
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Authorization endpoint (used by `ai-memory auth login`, not this crate).
pub const AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";

/// OpenAI public client ID shared by Codex CLI and OpenCode.
///
/// WARNING: this is OpenAI's first-party Codex CLI client ID, not a
/// registered ai-memory application client ID. OpenAI may restrict it to
/// first-party tools at any time — if that happens, users will need to
/// register their own OAuth app and supply the client ID via a config
/// override (not yet implemented). Source: Codex CLI source code and the
/// opencode project, both of which use this same hardcoded value.
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// OAuth scopes required for Chat Completions access via a ChatGPT
/// subscription. `offline_access` is critical — without it no refresh token
/// is issued and the user must re-authenticate every hour.
pub const OAUTH_SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

/// Persisted OAuth token state written to `<data_dir>/oauth_token.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthToken {
    /// Short-lived access token sent as `Bearer <token>`.
    pub access_token: String,
    /// Long-lived token used to mint new access tokens without user
    /// interaction. Some providers rotate this on each refresh — we always
    /// persist whatever the response returns.
    pub refresh_token: String,
    /// Unix timestamp (seconds) at which the access token expires.
    pub expires_at: u64,
    /// Always `"Bearer"` for OpenAI.
    pub token_type: String,
    /// Scopes granted by the authorization server.
    pub scope: String,
}

impl OAuthToken {
    /// True when the access token has expired or will expire within 60 s.
    #[must_use]
    pub fn needs_refresh(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now + 60 >= self.expires_at
    }

    /// Load from the token file. Returns `None` when the file doesn't exist.
    ///
    /// # Errors
    /// `LlmError::AuthExpired` if the file exists but cannot be parsed.
    pub fn load(path: &std::path::Path) -> LlmResult<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(path)
            .map_err(|e| LlmError::AuthExpired(format!("read token file: {e}")))?;
        serde_json::from_slice(&bytes)
            .map_err(|e| LlmError::AuthExpired(format!("parse token file: {e}")))
            .map(Some)
    }

    /// Atomically write to disk (tmp → rename) with mode 0600.
    ///
    /// # Errors
    /// Propagates IO errors as `LlmError::AuthExpired`.
    pub fn save(&self, path: &std::path::Path) -> LlmResult<()> {
        use std::io::Write as _;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| LlmError::AuthExpired(format!("create token dir: {e}")))?;
        }
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_vec_pretty(self).map_err(LlmError::from)?;
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)
                .map_err(|e| LlmError::AuthExpired(format!("open tmp token file: {e}")))?;
            // Set 0600 before writing so data never lands world-readable.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| LlmError::AuthExpired(format!("chmod token file: {e}")))?;
            }
            f.write_all(&json)
                .map_err(|e| LlmError::AuthExpired(format!("write token file: {e}")))?;
            f.sync_all()
                .map_err(|e| LlmError::AuthExpired(format!("fsync token file: {e}")))?;
        }
        std::fs::rename(&tmp, path)
            .map_err(|e| LlmError::AuthExpired(format!("rename token file: {e}")))
    }
}

/// OpenAI provider that authenticates via an OAuth 2.0 access token.
///
/// Wraps `OpenAiProvider` internally — the only difference from the API-key
/// path is that the Bearer token comes from a refreshable OAuth session.
/// Token refresh is on-demand: before each request we check `expires_at`;
/// if expired we exchange the refresh token for a new access token and
/// persist the result before proceeding.
pub struct OpenAiOAuthProvider {
    /// Shared HTTP client — same instance for both refresh calls and the
    /// inner `OpenAiProvider` built per-request.
    client: reqwest::Client,
    model: String,
    token_path: PathBuf,
    token: Mutex<OAuthToken>,
}

impl OpenAiOAuthProvider {
    /// Load the saved token from `token_path` and build the provider.
    ///
    /// # Errors
    /// - `LlmError::AuthExpired` when the token file doesn't exist (user
    ///   must run `ai-memory auth login` first).
    /// - Propagates IO / parse errors from [`OAuthToken::load`].
    pub fn new(token_path: PathBuf, model: impl Into<String>) -> LlmResult<Self> {
        let token = OAuthToken::load(&token_path)?.ok_or_else(|| {
            LlmError::AuthExpired(
                "no OAuth token found — run `ai-memory auth login` to authenticate".into(),
            )
        })?;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(LlmError::from)?;
        Ok(Self {
            client,
            model: model.into(),
            token_path,
            token: Mutex::new(token),
        })
    }

    /// Return a valid access token, refreshing if necessary.
    async fn current_token(&self) -> LlmResult<SecretString> {
        let mut guard = self.token.lock().await;
        if guard.needs_refresh() {
            info!("openai oauth token expired or near-expiry, refreshing");
            let refreshed = exchange_refresh_token(&self.client, &guard.refresh_token).await?;
            refreshed.save(&self.token_path)?;
            *guard = refreshed;
        }
        Ok(SecretString::new(guard.access_token.clone().into()))
    }
}

#[async_trait]
impl LlmProvider for OpenAiOAuthProvider {
    fn name(&self) -> &'static str {
        "openai-oauth"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let token = self.current_token().await?;
        let model = self.model.clone();
        let inner = OpenAiProvider::new(token, model)?.with_dialect(RequestDialect::Official);
        inner.complete(request).await
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        let token = self.current_token().await?;
        let model = self.model.clone();
        let inner = OpenAiProvider::new(token, model)?.with_dialect(RequestDialect::Official);
        inner.complete_structured_raw(request, schema).await
    }
}

/// Exchange a refresh token for a new `OAuthToken`.
async fn exchange_refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> LlmResult<OAuthToken> {
    #[derive(Serialize)]
    struct Body<'a> {
        grant_type: &'a str,
        client_id: &'a str,
        refresh_token: &'a str,
    }

    #[derive(Deserialize)]
    struct Response {
        access_token: String,
        /// Providers may rotate the refresh token. Absent means keep old one.
        refresh_token: Option<String>,
        expires_in: u64,
        token_type: String,
        scope: Option<String>,
    }

    let body = Body {
        grant_type: "refresh_token",
        client_id: CODEX_CLIENT_ID,
        refresh_token,
    };

    debug!(url = TOKEN_URL, "POST oauth refresh");
    let resp = client
        .post(TOKEN_URL)
        .json(&body)
        .send()
        .await
        .map_err(LlmError::from)?;

    let status = resp.status();
    if !status.is_success() {
        let body_text = resp.text().await.unwrap_or_default();
        return Err(LlmError::AuthExpired(format!(
            "token refresh failed ({status}): {body_text}. \
             Run `ai-memory auth login` to re-authenticate."
        )));
    }

    let r: Response = resp.json().await.map_err(LlmError::from)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Ok(OAuthToken {
        access_token: r.access_token,
        refresh_token: r.refresh_token.unwrap_or_else(|| refresh_token.to_string()),
        expires_at: now + r.expires_in,
        token_type: r.token_type,
        scope: r.scope.unwrap_or_else(|| OAUTH_SCOPES.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_refresh_true_when_expired() {
        let t = OAuthToken {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: 1, // far in the past
            token_type: "Bearer".into(),
            scope: OAUTH_SCOPES.into(),
        };
        assert!(t.needs_refresh());
    }

    #[test]
    fn needs_refresh_false_when_valid() {
        let far_future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 7200; // 2 hours from now
        let t = OAuthToken {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: far_future,
            token_type: "Bearer".into(),
            scope: OAUTH_SCOPES.into(),
        };
        assert!(!t.needs_refresh());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_token.json");
        let far_future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 3600;
        let token = OAuthToken {
            access_token: "access123".into(),
            refresh_token: "refresh456".into(),
            expires_at: far_future,
            token_type: "Bearer".into(),
            scope: OAUTH_SCOPES.into(),
        };
        token.save(&path).unwrap();
        let loaded = OAuthToken::load(&path).unwrap().unwrap();
        assert_eq!(loaded.access_token, "access123");
        assert_eq!(loaded.refresh_token, "refresh456");
        assert_eq!(loaded.expires_at, far_future);
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert!(OAuthToken::load(&path).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn save_sets_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_token.json");
        let far_future = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            + 3600;
        let token = OAuthToken {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: far_future,
            token_type: "Bearer".into(),
            scope: OAUTH_SCOPES.into(),
        };
        token.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "token file must be mode 0600");
    }
}
