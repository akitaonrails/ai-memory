//! GitHub Copilot provider.
//!
//! Copilot is not an OpenAI-compatible API-key provider. The stable auth chain
//! is GitHub user token -> `/copilot_internal/v2/token` -> short-lived Copilot
//! bearer token -> Copilot chat completions, with the same integration headers
//! used by Copilot Chat clients.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::auth::CopilotAuth;
use crate::auth_file::{load_entry, now_ms, save_entry};
use crate::error::{LlmError, LlmResult};
use crate::openai::{STRUCTURED_OUTPUT_SCHEMA_NAME, enforce_strict_object_schemas};
use crate::provider::LlmProvider;
use crate::response::{provider_error_body, response_json_limited};
use crate::types::{ChatRequest, ChatResponse, ExtraHeaders, Usage};

/// GitHub Copilot's public OAuth client id used by Copilot clients.
pub const GITHUB_COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

/// GitHub device-code endpoint.
pub const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";

/// GitHub OAuth token endpoint.
pub const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// GitHub endpoint that exchanges a GitHub user token for a Copilot API token.
pub const GITHUB_COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Default Copilot chat API base when a token does not advertise `proxy-ep`.
pub const DEFAULT_COPILOT_API_BASE_URL: &str = "https://api.githubcopilot.com";

/// Copilot integration id for chat requests and token exchange.
pub const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

/// VS Code-like editor version header expected by Copilot endpoints.
pub const COPILOT_EDITOR_VERSION: &str = "vscode/1.107.0";

/// VS Code-like plugin version header expected by Copilot endpoints.
pub const COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.35.0";

/// VS Code Copilot Chat user agent.
pub const COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.35.0";

/// GitHub API version used for the token exchange endpoint.
pub const COPILOT_GITHUB_API_VERSION: &str = "2025-04-01";

const COPILOT_REFRESH_MARGIN_MS: u64 = 120_000;

/// Stored GitHub Copilot auth state.
#[derive(Clone, Default)]
pub struct CopilotToken {
    /// Long-lived GitHub user token used for Copilot token exchange.
    pub github_access: Option<SecretString>,
    /// Optional GitHub token expiry in milliseconds since Unix epoch.
    pub github_expires_at_ms: Option<u64>,
    /// Cached short-lived Copilot API token.
    pub copilot_access: Option<SecretString>,
    /// Copilot API token expiry in milliseconds since Unix epoch.
    pub copilot_expires_at_ms: Option<u64>,
    /// Cached/derived Copilot API base URL.
    pub api_base_url: Option<String>,
}

impl std::fmt::Debug for CopilotToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopilotToken")
            .field(
                "github_access",
                &self.github_access.as_ref().map(|_| "<redacted>"),
            )
            .field("github_expires_at_ms", &self.github_expires_at_ms)
            .field(
                "copilot_access",
                &self.copilot_access.as_ref().map(|_| "<redacted>"),
            )
            .field("copilot_expires_at_ms", &self.copilot_expires_at_ms)
            .field("api_base_url", &self.api_base_url)
            .finish()
    }
}

impl CopilotToken {
    /// Build stored auth from a GitHub user token.
    #[must_use]
    pub fn from_github_token(token: impl Into<String>, expires_at_ms: Option<u64>) -> Self {
        Self {
            github_access: Some(SecretString::from(token.into())),
            github_expires_at_ms: expires_at_ms,
            ..Self::default()
        }
    }

    fn cached_copilot_token(&self) -> Option<CopilotApiToken> {
        let access = self.copilot_access.as_ref()?;
        let expires_at_ms = self.copilot_expires_at_ms?;
        if now_ms().saturating_add(COPILOT_REFRESH_MARGIN_MS) >= expires_at_ms {
            return None;
        }
        Some(CopilotApiToken {
            access: access.clone(),
            expires_at_ms,
            base_url: self
                .api_base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_COPILOT_API_BASE_URL.into()),
        })
    }

    /// True when a GitHub token is available and not known to be expired.
    #[must_use]
    pub fn has_refreshable_github_token(&self) -> bool {
        let Some(_token) = self.github_access.as_ref() else {
            return false;
        };
        self.github_expires_at_ms
            .is_none_or(|expires| now_ms().saturating_add(COPILOT_REFRESH_MARGIN_MS) < expires)
    }

    /// True when the cached Copilot API token is still usable without refresh.
    #[must_use]
    pub fn has_valid_cached_copilot_token(&self) -> bool {
        self.cached_copilot_token().is_some()
    }

    /// Load the Copilot token from a shared auth file.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file exists but cannot be read or
    /// parsed.
    pub fn load(path: &Path) -> LlmResult<Option<Self>> {
        let Some(entry) = load_entry::<CopilotEntry>(path, "copilot")? else {
            return Ok(None);
        };
        Ok(Some(Self {
            github_access: entry.github_access.map(SecretString::from),
            github_expires_at_ms: entry.github_expires,
            copilot_access: entry.copilot_access.map(SecretString::from),
            copilot_expires_at_ms: entry.copilot_expires,
            api_base_url: entry.api_base_url,
        }))
    }

    /// Save the token into the shared auth file, preserving unknown keys.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file cannot be written.
    pub fn save(&self, path: &Path) -> LlmResult<()> {
        let entry = CopilotEntry {
            kind: "oauth".into(),
            github_access: self
                .github_access
                .as_ref()
                .map(|s| s.expose_secret().to_string()),
            github_expires: self.github_expires_at_ms,
            copilot_access: self
                .copilot_access
                .as_ref()
                .map(|s| s.expose_secret().to_string()),
            copilot_expires: self.copilot_expires_at_ms,
            api_base_url: self.api_base_url.clone(),
        };
        save_entry(path, "copilot", Some(entry))
    }

    /// Remove the Copilot entry from the shared auth file.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file cannot be updated.
    pub fn remove(path: &Path) -> LlmResult<()> {
        save_entry::<CopilotEntry>(path, "copilot", None)
    }
}

/// Copilot provider backed by Copilot chat completions.
pub struct CopilotProvider {
    client: reqwest::Client,
    model: String,
    auth: CopilotAuth,
    stored: Mutex<CopilotToken>,
    timeout: Duration,
    extra_headers: ExtraHeaders,
}

impl CopilotProvider {
    /// Build a provider from resolved Copilot auth inputs.
    ///
    /// # Errors
    /// Returns [`LlmError::NotConfigured`] when no GitHub/direct token is
    /// available.
    pub fn new(auth: CopilotAuth, model: impl Into<String>) -> LlmResult<Self> {
        let stored = CopilotToken::load(&auth.token_file)?.unwrap_or_default();
        if auth.direct_api_token.is_none()
            && auth.github_token.is_none()
            && stored.github_access.is_none()
            && stored.cached_copilot_token().is_none()
        {
            return Err(LlmError::NotConfigured(format!(
                "no copilot auth found at {}; run `ai-memory auth login copilot` or set COPILOT_GITHUB_TOKEN",
                auth.token_file.display()
            )));
        }
        let client = reqwest::Client::builder()
            .user_agent(COPILOT_USER_AGENT)
            .build()
            .map_err(LlmError::from)?;
        Ok(Self {
            client,
            model: model.into(),
            auth,
            stored: Mutex::new(stored),
            timeout: Duration::from_secs(crate::DEFAULT_REQUEST_TIMEOUT_SECS),
            extra_headers: ExtraHeaders::default(),
        })
    }

    /// Override the per-request timeout (default
    /// [`crate::DEFAULT_REQUEST_TIMEOUT_SECS`]). Also bounds the
    /// GitHub token-exchange request.
    #[must_use]
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs);
        self
    }

    /// Attach operator-configured headers to every chat request. The factory
    /// calls this with `ProviderConfig::extra_headers`. Replaces the client's
    /// default `user-agent` when the operator configures one.
    #[must_use]
    pub fn with_extra_headers(mut self, headers: ExtraHeaders) -> Self {
        self.extra_headers = headers;
        self
    }

    async fn current_token(&self) -> LlmResult<CopilotApiToken> {
        if let Some(token) = self.auth.direct_api_token.as_ref() {
            return Ok(CopilotApiToken {
                access: token.clone(),
                expires_at_ms: u64::MAX,
                base_url: resolve_copilot_api_base_url(
                    self.auth.api_base_url.as_deref(),
                    token.expose_secret(),
                ),
            });
        }

        let mut guard = self.stored.lock().await;
        if let Some(mut cached) = guard.cached_copilot_token() {
            if let Some(url) = self.auth.api_base_url.clone() {
                cached.base_url = url;
            }
            return Ok(cached);
        }

        let github = self
            .auth
            .github_token
            .clone()
            .or_else(|| guard.github_access.clone())
            .ok_or_else(|| {
                LlmError::NotConfigured(
                    "copilot GitHub token missing; run `ai-memory auth login copilot` or set COPILOT_GITHUB_TOKEN"
                        .into(),
                )
            })?;

        info!("copilot API token expired or missing, exchanging GitHub token");
        let exchanged = exchange_copilot_token(&self.client, self.timeout, &github).await?;
        guard.copilot_access = Some(exchanged.access.clone());
        guard.copilot_expires_at_ms = Some(exchanged.expires_at_ms);
        guard.api_base_url = Some(resolve_copilot_api_base_url(
            self.auth.api_base_url.as_deref(),
            exchanged.access.expose_secret(),
        ));
        guard.save(&self.auth.token_file)?;

        Ok(CopilotApiToken {
            access: exchanged.access,
            expires_at_ms: exchanged.expires_at_ms,
            base_url: guard
                .api_base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_COPILOT_API_BASE_URL.into()),
        })
    }

    async fn post(&self, body: &CopilotChatRequest<'_>) -> LlmResult<CopilotChatResponse> {
        let token = self.current_token().await?;
        let url = format!("{}/chat/completions", token.base_url.trim_end_matches('/'));
        debug!(url = %url, "POST copilot chat completions");
        let request = self.extra_headers.apply(
            self.client
                .post(&url)
                .timeout(self.timeout)
                .bearer_auth(token.access.expose_secret())
                .headers(copilot_runtime_headers()),
        );
        let resp = request.json(body).send().await.map_err(LlmError::from)?;
        let status = resp.status();
        if !status.is_success() {
            let body = provider_error_body(resp).await;
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
        }
        response_json_limited::<CopilotChatResponse>(resp).await
    }
}

#[async_trait]
impl LlmProvider for CopilotProvider {
    fn name(&self) -> &'static str {
        "copilot"
    }

    fn model(&self) -> &str {
        &self.model
    }

    async fn complete(&self, request: ChatRequest) -> LlmResult<ChatResponse> {
        let response = self
            .post(&build_chat_request(&self.model, &request, None))
            .await?;
        Ok(to_chat_response(response))
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        mut schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        enforce_strict_object_schemas(&mut schema);
        let response_format = CopilotResponseFormat::JsonSchema {
            json_schema: CopilotJsonSchema {
                name: STRUCTURED_OUTPUT_SCHEMA_NAME.into(),
                schema,
                strict: true,
            },
        };
        let response = self
            .post(&build_chat_request(
                &self.model,
                &request,
                Some(response_format),
            ))
            .await?;
        let text = response
            .choices
            .first()
            .and_then(|c| c.message.content.as_deref())
            .unwrap_or("");
        serde_json::from_str::<serde_json::Value>(text).map_err(LlmError::from)
    }
}

#[derive(Debug, Clone)]
struct CopilotApiToken {
    access: SecretString,
    expires_at_ms: u64,
    base_url: String,
}

#[derive(Debug, Deserialize)]
struct CopilotExchangeResponse {
    token: String,
    expires_at: u64,
}

async fn exchange_copilot_token(
    client: &reqwest::Client,
    timeout: Duration,
    github_token: &SecretString,
) -> LlmResult<CopilotApiToken> {
    let resp = client
        .get(GITHUB_COPILOT_TOKEN_URL)
        .timeout(timeout)
        .bearer_auth(github_token.expose_secret())
        .headers(copilot_token_exchange_headers())
        .send()
        .await
        .map_err(LlmError::from)?;
    let status = resp.status();
    if !status.is_success() {
        let body = provider_error_body(resp).await;
        return Err(LlmError::Auth(format!(
            "copilot token exchange failed ({status}): {body}"
        )));
    }
    let exchanged = response_json_limited::<CopilotExchangeResponse>(resp).await?;
    let expires_at_ms = unix_epoch_to_ms(exchanged.expires_at);
    Ok(CopilotApiToken {
        base_url: derive_copilot_api_base_url_from_token(&exchanged.token)
            .unwrap_or_else(|| DEFAULT_COPILOT_API_BASE_URL.into()),
        access: SecretString::from(exchanged.token),
        expires_at_ms,
    })
}

fn build_chat_request<'a>(
    model: &'a str,
    request: &'a ChatRequest,
    response_format: Option<CopilotResponseFormat>,
) -> CopilotChatRequest<'a> {
    let mut messages = Vec::new();
    if let Some(sys) = request.system.as_deref() {
        messages.push(CopilotMsg {
            role: "system",
            content: sys,
        });
    }
    for m in &request.messages {
        messages.push(CopilotMsg {
            role: m.role.as_str(),
            content: &m.content,
        });
    }
    CopilotChatRequest {
        model,
        messages,
        max_tokens: Some(request.max_tokens),
        temperature: if model_uses_default_temperature(model) {
            None
        } else {
            request.temperature
        },
        stream: false,
        response_format,
    }
}

fn model_uses_default_temperature(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("gpt-5") || m.starts_with('o')
}

#[derive(Debug, Serialize)]
struct CopilotChatRequest<'a> {
    model: &'a str,
    messages: Vec<CopilotMsg<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<CopilotResponseFormat>,
}

#[derive(Debug, Serialize)]
struct CopilotMsg<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CopilotResponseFormat {
    JsonSchema { json_schema: CopilotJsonSchema },
}

#[derive(Debug, Serialize)]
struct CopilotJsonSchema {
    name: String,
    schema: serde_json::Value,
    strict: bool,
}

#[derive(Debug, Deserialize)]
struct CopilotChatResponse {
    choices: Vec<CopilotChoice>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<CopilotUsage>,
}

#[derive(Debug, Deserialize)]
struct CopilotChoice {
    message: CopilotMessageResponse,
}

#[derive(Debug, Deserialize)]
struct CopilotMessageResponse {
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CopilotUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

fn to_chat_response(response: CopilotChatResponse) -> ChatResponse {
    let text = response
        .choices
        .first()
        .and_then(|c| c.message.content.as_deref())
        .unwrap_or_default()
        .to_string();
    ChatResponse {
        text,
        usage: response.usage.map(|u| Usage {
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
        }),
        model: response.model.unwrap_or_else(|| "copilot".into()),
    }
}

fn copilot_token_exchange_headers() -> reqwest::header::HeaderMap {
    let mut headers = copilot_ide_headers(true);
    headers.insert(
        reqwest::header::HeaderName::from_static("copilot-integration-id"),
        reqwest::header::HeaderValue::from_static(COPILOT_INTEGRATION_ID),
    );
    headers
}

fn copilot_runtime_headers() -> reqwest::header::HeaderMap {
    let mut headers = copilot_ide_headers(false);
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        reqwest::header::CONTENT_TYPE,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("copilot-integration-id"),
        reqwest::header::HeaderValue::from_static(COPILOT_INTEGRATION_ID),
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("openai-organization"),
        reqwest::header::HeaderValue::from_static("github-copilot"),
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("x-initiator"),
        reqwest::header::HeaderValue::from_static("user"),
    );
    headers
}

fn copilot_ide_headers(include_api_version: bool) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        reqwest::header::ACCEPT_ENCODING,
        reqwest::header::HeaderValue::from_static("identity"),
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("editor-version"),
        reqwest::header::HeaderValue::from_static(COPILOT_EDITOR_VERSION),
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("editor-plugin-version"),
        reqwest::header::HeaderValue::from_static(COPILOT_EDITOR_PLUGIN_VERSION),
    );
    headers.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static(COPILOT_USER_AGENT),
    );
    if include_api_version {
        headers.insert(
            reqwest::header::HeaderName::from_static("x-github-api-version"),
            reqwest::header::HeaderValue::from_static(COPILOT_GITHUB_API_VERSION),
        );
    }
    headers
}

/// Derive Copilot API base URL from the `proxy-ep` field in a Copilot token.
#[must_use]
pub fn derive_copilot_api_base_url_from_token(token: &str) -> Option<String> {
    let proxy_ep = token.split(';').find_map(|field| {
        field
            .trim()
            .strip_prefix("proxy-ep=")
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })?;
    let url_text = if proxy_ep.starts_with("http://") || proxy_ep.starts_with("https://") {
        proxy_ep.to_string()
    } else {
        format!("https://{proxy_ep}")
    };
    let url = reqwest::Url::parse(&url_text).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host
        .strip_prefix("proxy.")
        .map(|rest| format!("api.{rest}"))
        .unwrap_or(host);
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    Some(format!("{}://{host}{port}", url.scheme()))
}

fn resolve_copilot_api_base_url(override_url: Option<&str>, token: &str) -> String {
    override_url
        .filter(|url| !url.trim().is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| derive_copilot_api_base_url_from_token(token))
        .unwrap_or_else(|| DEFAULT_COPILOT_API_BASE_URL.into())
}

#[derive(Debug, Serialize, Deserialize)]
struct CopilotEntry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "githubAccess", skip_serializing_if = "Option::is_none")]
    github_access: Option<String>,
    #[serde(rename = "githubExpires", skip_serializing_if = "Option::is_none")]
    github_expires: Option<u64>,
    #[serde(rename = "copilotAccess", skip_serializing_if = "Option::is_none")]
    copilot_access: Option<String>,
    #[serde(rename = "copilotExpires", skip_serializing_if = "Option::is_none")]
    copilot_expires: Option<u64>,
    #[serde(rename = "apiBaseUrl", skip_serializing_if = "Option::is_none")]
    api_base_url: Option<String>,
}

fn unix_epoch_to_ms(value: u64) -> u64 {
    if value > 100_000_000_000 {
        value
    } else {
        value.saturating_mul(1000)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn token_file_round_trips_copilot_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let token = CopilotToken {
            github_access: Some(SecretString::from("ghu-test")),
            github_expires_at_ms: None,
            copilot_access: Some(SecretString::from("copilot-test")),
            copilot_expires_at_ms: Some(1234),
            api_base_url: Some("https://api.example.test".into()),
        };

        token.save(&path).unwrap();
        let loaded = CopilotToken::load(&path).unwrap().unwrap();

        assert_eq!(loaded.github_access.unwrap().expose_secret(), "ghu-test");
        assert_eq!(
            loaded.copilot_access.unwrap().expose_secret(),
            "copilot-test"
        );
        assert_eq!(loaded.copilot_expires_at_ms, Some(1234));
        assert_eq!(
            loaded.api_base_url.as_deref(),
            Some("https://api.example.test")
        );
    }

    #[test]
    fn token_file_preserves_unknown_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "openai": { "type": "oauth", "access": "x", "refresh": "y", "expires": 1 }
            }))
            .unwrap(),
        )
        .unwrap();

        CopilotToken::from_github_token("ghu-test", None)
            .save(&path)
            .unwrap();
        let value =
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();

        assert!(value.get("openai").is_some());
        assert_eq!(value["copilot"]["githubAccess"], "ghu-test");
    }

    #[test]
    fn token_file_ignores_non_oauth_copilot_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "copilot": { "type": "api", "key": "token" }
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(CopilotToken::load(&path).unwrap().is_none());
    }

    #[test]
    fn remove_deletes_only_copilot_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "copilot": { "type": "oauth", "githubAccess": "ghu" },
                "other": { "type": "oauth", "access": "x" }
            }))
            .unwrap(),
        )
        .unwrap();

        CopilotToken::remove(&path).unwrap();
        let value =
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert!(value.get("copilot").is_none());
        assert!(value.get("other").is_some());
    }

    #[test]
    fn derives_api_base_url_from_proxy_ep() {
        assert_eq!(
            derive_copilot_api_base_url_from_token(
                "tid=abc;proxy-ep=https://proxy.individual.githubcopilot.com;exp=1:mac"
            ),
            Some("https://api.individual.githubcopilot.com".into())
        );
        assert_eq!(
            derive_copilot_api_base_url_from_token(
                "tid=abc;proxy-ep=proxy.enterprise.example.com:443;exp=1:mac"
            ),
            Some("https://api.enterprise.example.com".into())
        );
    }

    #[test]
    fn resolves_direct_token_base_url_from_override_or_proxy_ep() {
        let token = "tid=abc;proxy-ep=https://proxy.individual.githubcopilot.com;exp=1:mac";

        assert_eq!(
            resolve_copilot_api_base_url(Some("https://api.override.test"), token),
            "https://api.override.test"
        );
        assert_eq!(
            resolve_copilot_api_base_url(None, token),
            "https://api.individual.githubcopilot.com"
        );
        assert_eq!(
            resolve_copilot_api_base_url(None, "no-proxy"),
            DEFAULT_COPILOT_API_BASE_URL
        );
    }

    #[test]
    fn status_helpers_reject_expired_unrefreshable_cache() {
        let expired = now_ms().saturating_sub(10_000);
        let token = CopilotToken {
            github_access: None,
            github_expires_at_ms: None,
            copilot_access: Some(SecretString::from("cached")),
            copilot_expires_at_ms: Some(expired),
            api_base_url: None,
        };

        assert!(!token.has_refreshable_github_token());
        assert!(!token.has_valid_cached_copilot_token());
    }

    #[test]
    fn status_helpers_accept_refreshable_github_token() {
        let token = CopilotToken::from_github_token("ghu-test", None);

        assert!(token.has_refreshable_github_token());
        assert!(!token.has_valid_cached_copilot_token());
    }

    #[test]
    fn token_exchange_headers_include_vscode_chat_scope() {
        let headers = copilot_token_exchange_headers();
        assert_eq!(
            headers.get("Copilot-Integration-Id").unwrap(),
            COPILOT_INTEGRATION_ID
        );
        assert_eq!(
            headers.get("X-GitHub-Api-Version").unwrap(),
            COPILOT_GITHUB_API_VERSION
        );
    }

    #[test]
    fn runtime_headers_include_copilot_org() {
        let headers = copilot_runtime_headers();
        assert_eq!(
            headers.get("Copilot-Integration-Id").unwrap(),
            COPILOT_INTEGRATION_ID
        );
        assert_eq!(
            headers.get("Openai-Organization").unwrap(),
            "github-copilot"
        );
    }

    #[test]
    fn chat_request_uses_openai_compatible_shape() {
        let request = ChatRequest {
            system: Some("sys".into()),
            messages: vec![crate::types::ChatMessage::user("hello")],
            temperature: Some(0.2),
            max_tokens: 123,
        };
        let value = serde_json::to_value(build_chat_request("gpt-5.5", &request, None)).unwrap();
        assert_eq!(value["messages"][0]["role"], "system");
        assert_eq!(value["messages"][1]["content"], "hello");
        assert_eq!(value["max_tokens"], 123);
        assert!(value.get("temperature").is_none());
        assert_eq!(value["stream"], false);
    }

    #[test]
    fn structured_request_uses_json_schema_format() {
        let request = ChatRequest::user_prompt("json please");
        let response_format = CopilotResponseFormat::JsonSchema {
            json_schema: CopilotJsonSchema {
                name: STRUCTURED_OUTPUT_SCHEMA_NAME.into(),
                schema: json!({ "type": "object", "properties": {} }),
                strict: true,
            },
        };
        let value = serde_json::to_value(build_chat_request(
            "gpt-5.5",
            &request,
            Some(response_format),
        ))
        .unwrap();
        assert_eq!(value["response_format"]["type"], "json_schema");
        assert_eq!(value["response_format"]["json_schema"]["name"], "Result");
        assert_eq!(value["response_format"]["json_schema"]["strict"], true);
    }

    #[test]
    fn epoch_seconds_convert_to_millis() {
        assert_eq!(unix_epoch_to_ms(1_700_000_000), 1_700_000_000_000);
        assert_eq!(unix_epoch_to_ms(1_700_000_000_000), 1_700_000_000_000);
    }
}
