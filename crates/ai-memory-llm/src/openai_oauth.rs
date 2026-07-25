//! OpenAI ChatGPT/Codex OAuth provider.
//!
//! This is intentionally not a thin wrapper around `api.openai.com`: ChatGPT
//! OAuth access tokens are Codex/ChatGPT credentials, not Platform API keys.
//! Requests go to the Codex Responses backend and include the account id when
//! the token exposes one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::auth_file::now_ms;
use crate::error::{LlmError, LlmResult};
use crate::openai::{STRUCTURED_OUTPUT_SCHEMA_NAME, enforce_strict_object_schemas};
use crate::provider::LlmProvider;
use crate::response::{provider_error_body, response_json_limited, response_text_limited};
use crate::stored_token::{StoredOAuthToken, refresh_grant};
use crate::text::truncate_with_ellipsis;
use crate::types::{ChatRequest, ChatResponse, Role, Usage};

/// OpenAI OAuth issuer used by Codex/OpenCode.
pub const OPENAI_OAUTH_ISSUER: &str = "https://auth.openai.com";

/// OpenAI OAuth authorization endpoint.
pub const OPENAI_OAUTH_AUTH_URL: &str = "https://auth.openai.com/oauth/authorize";

/// OpenAI OAuth token endpoint.
pub const OPENAI_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// ChatGPT/Codex Responses backend.
pub const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Public Codex/OpenCode OAuth client id.
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Scopes used by Codex/OpenCode for ChatGPT sign-in.
pub const OAUTH_SCOPES: &str = "openid profile email offline_access";

/// Status code used when the Codex SSE stream carries a terminal error
/// event (`response.failed`/`response.incomplete`/`response.cancelled`
/// or a top-level `error` payload). 502 ("Bad Gateway") models the
/// situation accurately: the upstream connected, but the response it
/// sent was not a usable completion — the Codex backend, not
/// ai-memory's HTTP layer, produced the failure.
const SSE_TERMINAL_ERROR_STATUS: u16 = 502;

/// Trim length for the body field of an SSE-terminal error so we
/// don't echo a multi-megabyte upstream error into the logs / chain.
/// Matches the 1024 used by `OpenAiProvider::post` for HTTP 4xx/5xx
/// bodies — same defensive cap, same reasoning.
const SSE_ERROR_BODY_TRIM: usize = 1024;

/// OpenAI-specific claim persisted next to the shared token triple.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OpenAiExtras {
    /// ChatGPT account/workspace id, when present in the token claims.
    #[serde(rename = "accountId", skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

/// Stored OpenAI OAuth token.
pub type OpenAiOAuthToken = StoredOAuthToken<OpenAiExtras>;

impl OpenAiOAuthToken {
    /// Build a token from an OAuth token response.
    #[must_use]
    pub fn from_token_response(
        access: impl Into<String>,
        refresh: impl Into<String>,
        expires_in_secs: u64,
        id_token: Option<&str>,
        previous_account_id: Option<String>,
    ) -> Self {
        let access = access.into();
        let account_id = id_token
            .and_then(extract_account_id_from_jwt)
            .or_else(|| extract_account_id_from_jwt(&access))
            .or(previous_account_id);
        Self {
            access: SecretString::from(access),
            refresh: SecretString::from(refresh.into()),
            expires_at_ms: now_ms().saturating_add(expires_in_secs.saturating_mul(1000)),
            extra: OpenAiExtras { account_id },
        }
    }

    /// Load the OpenAI OAuth token from a shared token file.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file exists but cannot be read or
    /// parsed.
    pub fn load(path: &Path) -> LlmResult<Option<Self>> {
        Self::load_key(path, "openai")
    }

    /// Save the token into the shared token file, preserving unknown keys.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file cannot be written.
    pub fn save(&self, path: &Path) -> LlmResult<()> {
        self.save_key(path, "openai")
    }

    /// Remove the OpenAI entry from the shared token file.
    ///
    /// # Errors
    /// Returns [`LlmError::Auth`] when the file cannot be updated.
    pub fn remove(path: &Path) -> LlmResult<()> {
        Self::remove_key(path, "openai")
    }
}

/// OAuth token response returned by `auth.openai.com`.
#[derive(Debug, Deserialize)]
pub struct OpenAiOAuthTokenResponse {
    /// Access token.
    pub access_token: String,
    /// Refresh token. Refresh responses may omit it, meaning keep old refresh.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Expires-in seconds. Codex/OpenCode default to one hour when omitted.
    #[serde(default)]
    pub expires_in: Option<u64>,
    /// Optional ID token carrying ChatGPT account claims.
    #[serde(default)]
    pub id_token: Option<String>,
}

/// OpenAI OAuth provider backed by the ChatGPT/Codex Responses endpoint.
pub struct OpenAiOAuthProvider {
    client: reqwest::Client,
    model: String,
    token_path: PathBuf,
    token: Mutex<OpenAiOAuthToken>,
}

impl OpenAiOAuthProvider {
    /// Build a provider from an OAuth token file.
    ///
    /// # Errors
    /// Returns [`LlmError::NotConfigured`] when no token is present.
    pub fn new(token_path: PathBuf, model: impl Into<String>) -> LlmResult<Self> {
        let token = OpenAiOAuthToken::load(&token_path)?.ok_or_else(|| {
            LlmError::NotConfigured(format!(
                "no openai-oauth token found at {}; run `ai-memory auth login openai-oauth`",
                token_path.display()
            ))
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

    async fn current_token(&self) -> LlmResult<OpenAiOAuthToken> {
        let mut guard = self.token.lock().await;
        if guard.needs_refresh() {
            info!("openai-oauth token expired or near-expiry, refreshing");
            let refreshed = refresh_access_token(&self.client, &guard).await?;
            refreshed.save(&self.token_path)?;
            *guard = refreshed;
        }
        Ok(guard.clone())
    }

    async fn post(&self, body: &CodexResponsesRequest<'_>) -> LlmResult<CodexResponsesResponse> {
        let token = self.current_token().await?;
        debug!(
            url = CODEX_RESPONSES_URL,
            "POST openai-oauth codex responses"
        );
        let mut request = self
            .client
            .post(CODEX_RESPONSES_URL)
            .bearer_auth(token.access.expose_secret())
            .header("content-type", "application/json")
            .header(
                "accept",
                if body.stream {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .header("openai-beta", "responses=experimental")
            .header("originator", "codex_cli_rs")
            .header("session_id", uuid::Uuid::new_v4().to_string())
            .json(body);
        if let Some(account_id) = token.extra.account_id.as_deref() {
            request = request.header("chatgpt-account-id", account_id);
        }
        let resp = request.send().await.map_err(LlmError::from)?;
        let status = resp.status();
        if !status.is_success() {
            let body = provider_error_body(resp).await;
            return Err(LlmError::Provider {
                status: status.as_u16(),
                body,
            });
        }
        if body.stream {
            parse_sse_response(&response_text_limited(resp).await?)
        } else {
            response_json_limited::<CodexResponsesResponse>(resp).await
        }
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
        let response = self
            .post(&build_request(&self.model, &request, None))
            .await?;
        Ok(into_chat_response(response))
    }

    async fn complete_structured_raw(
        &self,
        request: ChatRequest,
        mut schema: serde_json::Value,
    ) -> LlmResult<serde_json::Value> {
        enforce_strict_object_schemas(&mut schema);
        let response_format = CodexText {
            format: CodexTextFormat::JsonSchema {
                name: STRUCTURED_OUTPUT_SCHEMA_NAME.into(),
                schema,
                strict: true,
            },
        };
        let response = self
            .post(&build_request(&self.model, &request, Some(response_format)))
            .await?;
        let text = extract_output_text(&response).unwrap_or_default();
        serde_json::from_str::<serde_json::Value>(&text).map_err(LlmError::from)
    }
}

async fn refresh_access_token(
    client: &reqwest::Client,
    current: &OpenAiOAuthToken,
) -> LlmResult<OpenAiOAuthToken> {
    let token_response = refresh_grant::<OpenAiOAuthTokenResponse>(
        client,
        OPENAI_OAUTH_TOKEN_URL,
        current.refresh.expose_secret(),
        CODEX_CLIENT_ID,
        "openai-oauth refresh failed",
        "Run `ai-memory auth login openai-oauth` again.",
    )
    .await?;
    Ok(OpenAiOAuthToken::from_token_response(
        token_response.access_token,
        token_response
            .refresh_token
            .unwrap_or_else(|| current.refresh.expose_secret().to_string()),
        token_response.expires_in.unwrap_or(3600),
        token_response.id_token.as_deref(),
        current.extra.account_id.clone(),
    ))
}

fn build_request<'a>(
    model: &'a str,
    request: &'a ChatRequest,
    text: Option<CodexText>,
) -> CodexResponsesRequest<'a> {
    let input = request
        .messages
        .iter()
        .map(|msg| CodexInputMessage {
            role: match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            },
            content: vec![CodexInputContent {
                kind: "input_text",
                text: &msg.content,
            }],
        })
        .collect();
    CodexResponsesRequest {
        model,
        instructions: request.system.as_deref(),
        input,
        // The ChatGPT/Codex backend currently rejects `max_output_tokens`
        // for OAuth callers on this endpoint, so rely on the server default.
        max_output_tokens: None,
        temperature: if model_uses_default_temperature(model) {
            None
        } else {
            request.temperature
        },
        store: false,
        stream: true,
        text,
    }
}

fn parse_sse_response(body: &str) -> LlmResult<CodexResponsesResponse> {
    let mut current_event: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    let mut output_text = String::new();
    let mut completed: Option<CodexResponsesResponse> = None;

    let mut flush_event = |event: Option<&str>, data_lines: &mut Vec<&str>| -> LlmResult<()> {
        if data_lines.is_empty() {
            return Ok(());
        }
        let data = data_lines.join("\n");
        data_lines.clear();
        let trimmed = data.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return Ok(());
        }

        let value = serde_json::from_str::<serde_json::Value>(trimmed)?;
        // A bare `data: {"error": {...}}` event without a `type` field
        // and without an `event:` header would otherwise fall through
        // the `_ => {}` arm and silently disappear, leaving the caller
        // with "stream closed before response.completed". Promote
        // any top-level `error` payload to a terminal error so the real
        // upstream failure reaches the caller verbatim.
        if value.get("error").is_some() {
            return Err(LlmError::Provider {
                status: SSE_TERMINAL_ERROR_STATUS,
                body: truncate_with_ellipsis(trimmed, SSE_ERROR_BODY_TRIM),
            });
        }
        let kind = value
            .get("type")
            .and_then(|v| v.as_str())
            .or(event)
            .unwrap_or_default();
        match kind {
            "response.output_text.delta" => {
                if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
                    output_text.push_str(delta);
                }
            }
            "response.completed" => {
                let response = value.get("response").cloned().unwrap_or(value);
                completed = Some(serde_json::from_value(response)?);
            }
            "response.failed" | "response.incomplete" | "response.cancelled" | "error" => {
                return Err(LlmError::Provider {
                    status: SSE_TERMINAL_ERROR_STATUS,
                    body: truncate_with_ellipsis(trimmed, SSE_ERROR_BODY_TRIM),
                });
            }
            _ => {}
        }
        Ok(())
    };

    for line in body.lines() {
        if line.is_empty() {
            flush_event(current_event.as_deref(), &mut data_lines)?;
            current_event = None;
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            current_event = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start());
        }
    }
    // Final flush. A stream that ended mid-`data:` chunk leaves the
    // last partial JSON in `data_lines`; surface that as a
    // truncated-stream error so the caller doesn't see a generic
    // serde parse failure when the true cause is the upstream socket
    // closing before sending the closing `}`.
    if let Err(err) = flush_event(current_event.as_deref(), &mut data_lines) {
        return match (&err, completed.is_some()) {
            (LlmError::Serde(_), false) => Err(LlmError::UnexpectedShape(
                "openai-oauth stream truncated before final event closed (incomplete JSON payload)"
                    .into(),
            )),
            _ => Err(err),
        };
    }

    let mut response = completed.ok_or_else(|| {
        LlmError::UnexpectedShape("openai-oauth stream closed before response.completed".into())
    })?;
    if response
        .output_text
        .as_deref()
        .unwrap_or_default()
        .is_empty()
        && !output_text.is_empty()
    {
        response.output_text = Some(output_text);
    }
    Ok(response)
}

fn model_uses_default_temperature(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.starts_with("gpt-5") || m.starts_with('o')
}

#[derive(Debug, Serialize)]
struct CodexResponsesRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<&'a str>,
    input: Vec<CodexInputMessage<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    store: bool,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<CodexText>,
}

#[derive(Debug, Serialize)]
struct CodexInputMessage<'a> {
    role: &'a str,
    content: Vec<CodexInputContent<'a>>,
}

#[derive(Debug, Serialize)]
struct CodexInputContent<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    text: &'a str,
}

#[derive(Debug, Serialize)]
struct CodexText {
    format: CodexTextFormat,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CodexTextFormat {
    JsonSchema {
        name: String,
        schema: serde_json::Value,
        strict: bool,
    },
}

#[derive(Debug, Deserialize)]
struct CodexResponsesResponse {
    #[serde(default)]
    output_text: Option<String>,
    #[serde(default)]
    output: Vec<CodexOutputItem>,
    #[serde(default)]
    usage: Option<CodexUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexOutputItem {
    #[serde(default)]
    content: Vec<CodexOutputContent>,
}

#[derive(Debug, Deserialize)]
struct CodexOutputContent {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

fn into_chat_response(response: CodexResponsesResponse) -> ChatResponse {
    let model = response
        .model
        .clone()
        .unwrap_or_else(|| "openai-oauth".into());
    ChatResponse {
        text: extract_output_text(&response).unwrap_or_default(),
        usage: response.usage.map(|u| Usage {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
        }),
        model,
    }
}

fn extract_output_text(response: &CodexResponsesResponse) -> Option<String> {
    if let Some(text) = response
        .output_text
        .as_deref()
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_string());
    }
    response
        .output
        .iter()
        .flat_map(|item| item.content.iter())
        .filter_map(|content| content.text.as_deref())
        .find(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

fn extract_account_id_from_jwt(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let value = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    value
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            value
                .get("https://api.openai.com/auth")
                .and_then(|v| v.get("chatgpt_account_id"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            value
                .get("organizations")
                .and_then(|v| v.as_array())
                .and_then(|v| v.first())
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
        })
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret as _;
    use serde_json::json;

    use super::*;

    #[test]
    fn token_file_preserves_unknown_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_token.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "other": { "type": "oauth", "access": "x" }
            }))
            .unwrap(),
        )
        .unwrap();
        let token = OpenAiOAuthToken {
            access: SecretString::from("access"),
            refresh: SecretString::from("refresh"),
            expires_at_ms: 1234,
            extra: OpenAiExtras {
                account_id: Some("acct".into()),
            },
        };
        token.save(&path).unwrap();
        let value =
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert!(value.get("other").is_some());
        assert_eq!(value["openai"]["access"], "access");
        assert_eq!(value["openai"]["accountId"], "acct");
    }

    #[test]
    fn token_file_round_trips_openai_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_token.json");
        let token = OpenAiOAuthToken {
            access: SecretString::from("access"),
            refresh: SecretString::from("refresh"),
            expires_at_ms: 1234,
            extra: OpenAiExtras { account_id: None },
        };
        token.save(&path).unwrap();
        let loaded = OpenAiOAuthToken::load(&path).unwrap().unwrap();
        assert_eq!(loaded.access.expose_secret(), "access");
        assert_eq!(loaded.refresh.expose_secret(), "refresh");
        assert_eq!(loaded.expires_at_ms, 1234);
        // On-disk format: `accountId` is omitted entirely when absent.
        let value =
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert!(value["openai"].get("accountId").is_none());
    }

    #[test]
    fn token_file_ignores_non_oauth_openai_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_token.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "openai": { "type": "api", "key": "sk-test" }
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(OpenAiOAuthToken::load(&path).unwrap().is_none());
    }

    #[test]
    fn remove_deletes_only_openai_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_token.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "openai": { "type": "oauth", "access": "a", "refresh": "r", "expires": 1 },
                "other": { "type": "oauth", "access": "x" }
            }))
            .unwrap(),
        )
        .unwrap();
        OpenAiOAuthToken::remove(&path).unwrap();
        let value =
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert!(value.get("openai").is_none());
        assert!(value.get("other").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oauth_token.json");
        let token = OpenAiOAuthToken {
            access: SecretString::from("access"),
            refresh: SecretString::from("refresh"),
            expires_at_ms: 1234,
            extra: OpenAiExtras { account_id: None },
        };
        token.save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn jwt_account_id_extraction_uses_supported_fallbacks() {
        fn jwt(payload: serde_json::Value) -> String {
            let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(serde_json::to_vec(&payload).unwrap());
            format!("x.{payload}.y")
        }
        assert_eq!(
            extract_account_id_from_jwt(&jwt(json!({ "chatgpt_account_id": "top" }))),
            Some("top".into())
        );
        assert_eq!(
            extract_account_id_from_jwt(&jwt(json!({
                "https://api.openai.com/auth": { "chatgpt_account_id": "nested" }
            }))),
            Some("nested".into())
        );
        assert_eq!(
            extract_account_id_from_jwt(&jwt(json!({ "organizations": [{ "id": "org" }] }))),
            Some("org".into())
        );
    }

    #[test]
    fn codex_request_moves_system_to_instructions_and_omits_gpt5_temperature() {
        let request = ChatRequest {
            system: Some("sys".into()),
            messages: vec![crate::types::ChatMessage {
                role: Role::User,
                content: "hello".into(),
            }],
            temperature: Some(0.2),
            max_tokens: 123,
        };
        let value = serde_json::to_value(build_request("gpt-5.5", &request, None)).unwrap();
        assert_eq!(value["instructions"], "sys");
        assert_eq!(value["input"][0]["content"][0]["type"], "input_text");
        assert!(value.get("max_output_tokens").is_none());
        assert!(value.get("temperature").is_none());
        assert_eq!(value["store"], false);
        assert_eq!(value["stream"], true);
    }

    #[test]
    fn parse_sse_response_reconstructs_completed_payload() {
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n",
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.5\",\"usage\":{\"input_tokens\":12,\"output_tokens\":3},\"output\":[]}}\n\n",
            "data: [DONE]\n\n"
        );

        let response = parse_sse_response(body).unwrap();
        assert_eq!(response.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(response.output_text.as_deref(), Some("hello"));
        let usage = response.usage.expect("usage from completed event");
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 3);
    }

    #[test]
    fn parse_sse_response_surfaces_terminal_failure_events() {
        for event in [
            "response.failed",
            "response.incomplete",
            "response.cancelled",
            "error",
        ] {
            let body = format!(
                "event: {event}\n\
                 data: {{\"type\":\"{event}\",\"error\":{{\"message\":\"stream stopped\"}}}}\n\n"
            );

            let err = parse_sse_response(&body).expect_err("terminal event must fail");
            match err {
                LlmError::Provider { status, body } => {
                    assert_eq!(status, 502);
                    assert!(body.contains(event), "body should include event: {body}");
                    assert!(
                        body.contains("stream stopped"),
                        "body should include error: {body}"
                    );
                }
                other => panic!("expected provider error for {event}, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_sse_response_requires_completed_event() {
        let body = concat!(
            "event: response.output_text.delta\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "data: [DONE]\n\n",
        );

        let err = parse_sse_response(body).expect_err("missing completed event must fail");
        assert!(
            err.to_string()
                .contains("stream closed before response.completed"),
            "unexpected error: {err}"
        );
    }

    /// Audit BLOCKING fix: a `data: {"error":{...}}` event without a
    /// `type` field and without a preceding `event:` header used to
    /// fall through the kind-dispatch silently. Now any top-level
    /// `error` payload is promoted to a terminal error so the real
    /// upstream failure reaches the caller.
    #[test]
    fn parse_sse_response_promotes_top_level_error_without_event_header() {
        let body = "data: {\"error\":{\"message\":\"context length exceeded\"}}\n\n";
        let err = parse_sse_response(body)
            .expect_err("top-level error payload must surface as a terminal error");
        match err {
            LlmError::Provider { status, body } => {
                assert_eq!(status, super::SSE_TERMINAL_ERROR_STATUS);
                assert!(
                    body.contains("context length exceeded"),
                    "body must carry upstream error message: {body}"
                );
            }
            other => panic!("expected Provider error, got {other:?}"),
        }
    }

    /// Audit BLOCKING fix: a stream that ends mid-`data:` JSON (the
    /// upstream socket closes before the closing brace) used to surface
    /// as a generic serde parse error. The post-loop flush now catches
    /// that case when no `response.completed` event has arrived and
    /// emits the actionable "stream truncated" diagnostic.
    #[test]
    fn parse_sse_response_truncated_data_surfaces_as_truncated_stream() {
        // `data:` line carrying an unterminated JSON object, no blank
        // line after — the post-loop flush is what runs.
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel";
        let err = parse_sse_response(body).expect_err("truncated stream must fail");
        assert!(
            err.to_string()
                .contains("stream truncated before final event closed"),
            "expected truncation-aware error, got: {err}"
        );
    }

    /// Empty body — no `data:` lines at all — falls through to the
    /// `completed.is_none()` branch with the original
    /// "stream closed before response.completed" error.
    #[test]
    fn parse_sse_response_empty_body_reports_no_completed_event() {
        let err = parse_sse_response("").expect_err("empty body must fail");
        assert!(
            err.to_string()
                .contains("stream closed before response.completed"),
            "expected canonical 'no completed' error, got: {err}"
        );
    }

    /// SSE spec allows an event to carry multiple `data:` lines; the
    /// parser joins them with `\n` before JSON-decoding. Exercise the
    /// continuation path so a future refactor doesn't drop it.
    #[test]
    fn parse_sse_response_handles_multi_line_data_continuation() {
        // Real upstreams sometimes split a wide JSON object across
        // multiple `data:` lines; the parser joins on '\n' before
        // serde_json parses, so a JSON value spanning newlines must
        // survive (here we exercise a JSON STRING that contains a
        // literal newline, which IS valid serde input).
        let body = concat!(
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-5.5\",\"output_text\":\"line1\\nline2\",\"output\":[]}}\n\n",
            "data: [DONE]\n\n",
        );
        let resp = parse_sse_response(body).expect("multi-line content must parse");
        assert_eq!(resp.output_text.as_deref(), Some("line1\nline2"));
    }

    #[test]
    fn structured_request_uses_responses_json_schema_format() {
        let request = ChatRequest::user_prompt("json please");
        let text = CodexText {
            format: CodexTextFormat::JsonSchema {
                name: "Result".into(),
                schema: json!({ "type": "object", "properties": {} }),
                strict: true,
            },
        };
        let value = serde_json::to_value(build_request("gpt-5.5", &request, Some(text))).unwrap();
        assert_eq!(value["text"]["format"]["type"], "json_schema");
        assert_eq!(value["text"]["format"]["name"], "Result");
        assert_eq!(value["text"]["format"]["strict"], true);
    }

    #[test]
    fn response_text_prefers_output_text_then_output_items() {
        let direct: CodexResponsesResponse = serde_json::from_value(json!({
            "output_text": "direct",
            "output": [{ "content": [{ "text": "nested" }] }]
        }))
        .unwrap();
        assert_eq!(extract_output_text(&direct).as_deref(), Some("direct"));

        let nested: CodexResponsesResponse = serde_json::from_value(json!({
            "output": [{ "content": [{ "text": "nested" }] }]
        }))
        .unwrap();
        assert_eq!(extract_output_text(&nested).as_deref(), Some("nested"));
    }
}
