//! Shared transport for the ChatGPT/Codex Responses backend.

use std::time::Duration;

use secrecy::{ExposeSecret as _, SecretString};
use tracing::debug;

use crate::error::{LlmError, LlmResult};
use crate::openai_oauth::{CodexResponsesRequest, CodexResponsesResponse, parse_sse_response};
use crate::response::{provider_error_body, response_json_limited, response_text_limited};
use crate::types::ExtraHeaders;

/// ChatGPT/Codex Responses backend.
pub const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

pub(crate) struct CodexResponsesAuth<'a> {
    pub(crate) access_token: &'a SecretString,
    pub(crate) account_id: Option<&'a str>,
}

pub(crate) async fn post_codex_responses(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
    auth: &CodexResponsesAuth<'_>,
    extra_headers: &ExtraHeaders,
    body: &CodexResponsesRequest<'_>,
) -> LlmResult<CodexResponsesResponse> {
    debug!(url, "POST codex responses");
    let mut request = client
        .post(url)
        .timeout(timeout)
        .bearer_auth(auth.access_token.expose_secret())
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
    request = extra_headers.apply(request);
    if let Some(account_id) = auth.account_id {
        request = request.header("chatgpt-account-id", account_id);
    }
    let response = request.send().await.map_err(LlmError::from)?;
    let status = response.status();
    if !status.is_success() {
        return Err(LlmError::Provider {
            status: status.as_u16(),
            body: provider_error_body(response).await,
        });
    }
    if body.stream {
        parse_sse_response(&response_text_limited(response).await?)
    } else {
        response_json_limited::<CodexResponsesResponse>(response).await
    }
}
