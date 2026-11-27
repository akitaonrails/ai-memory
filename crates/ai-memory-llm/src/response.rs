use std::time::Duration;

use ai_memory_core::Sanitizer;
use serde::de::DeserializeOwned;

use crate::error::{LlmError, LlmResult};
use crate::text::truncate_with_ellipsis;

/// Generous cap for successful provider responses. Normal chat, structured
/// output, SSE transcripts, and embedding payloads are far smaller; this only
/// protects us from broken or hostile provider endpoints buffering forever.
pub(crate) const MAX_PROVIDER_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROVIDER_ERROR_BYTES: usize = 1024 * 1024;
const DISPLAY_ERROR_BYTES: usize = 1024;

pub(crate) async fn response_bytes_limited(
    mut resp: reqwest::Response,
    max_bytes: usize,
) -> LlmResult<Vec<u8>> {
    let status = resp.status().as_u16();
    let mut bytes = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(LlmError::from)? {
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(LlmError::Provider {
                status,
                body: format!("provider response exceeded {max_bytes} bytes"),
                retry_after: None,
            });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(crate) async fn response_text_limited(resp: reqwest::Response) -> LlmResult<String> {
    let bytes = response_bytes_limited(resp, MAX_PROVIDER_RESPONSE_BYTES).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub(crate) async fn response_json_limited<T: DeserializeOwned>(
    resp: reqwest::Response,
) -> LlmResult<T> {
    let bytes = response_bytes_limited(resp, MAX_PROVIDER_RESPONSE_BYTES).await?;
    serde_json::from_slice(&bytes).map_err(LlmError::from)
}

/// Read a failed response's body for an error message: at most 1 MiB is
/// read, secrets and terminal control sequences are scrubbed, and at most
/// 1 KiB is kept.
///
/// Gateways and providers echo request content in error bodies, and the
/// result is printed to a terminal or carried in [`LlmError::Provider`].
/// Scrubbing runs before the cut so a secret straddling the 1 KiB boundary
/// is still redacted rather than left as an unrecognisable prefix.
pub async fn provider_error_body(resp: reqwest::Response) -> String {
    match provider_error(resp).await {
        LlmError::Provider { body, .. } => body,
        error => format!("<failed to read provider error body: {error}>"),
    }
}

/// Scrub provider-controlled text with the built-in sanitizer, then bound it
/// for display.
///
/// Use this for a provider body that did not come through [`provider_error`]:
/// a parse failure on a 2xx response, or a terminal SSE frame. Those paths
/// build their own error body, and row 7f requires every error body to be
/// scrubbed before it can reach the health snapshot or the admin `llm-test`
/// route.
pub(crate) fn scrubbed_body(text: &str, limit: usize) -> String {
    truncate_with_ellipsis(&Sanitizer::builtin().scrub(text), limit)
}

/// Build a typed provider error while preserving a retry hint that may live
/// beyond the bounded display prefix. The body is still read at most 1 MiB,
/// scrubbed before parsing, and exposed at most 1 KiB.
pub async fn provider_error(resp: reqwest::Response) -> LlmError {
    let status = resp.status().as_u16();
    let header_retry = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_delta_seconds);
    match response_bytes_limited(resp, MAX_PROVIDER_ERROR_BYTES).await {
        Ok(bytes) => {
            let scrubbed = Sanitizer::builtin().scrub(&String::from_utf8_lossy(&bytes));
            let retry_after = header_retry.or_else(|| google_retry_delay(&scrubbed));
            LlmError::Provider {
                status,
                body: truncate_with_ellipsis(&scrubbed, DISPLAY_ERROR_BYTES),
                retry_after,
            }
        }
        Err(LlmError::Provider { body, .. }) => LlmError::Provider {
            status,
            body,
            retry_after: header_retry,
        },
        Err(error) => LlmError::Provider {
            status,
            body: format!("<failed to read provider error body: {error}>"),
            retry_after: header_retry,
        },
    }
}

fn parse_delta_seconds(raw: &str) -> Option<Duration> {
    raw.trim().parse::<u64>().ok().map(Duration::from_secs)
}

fn google_retry_delay(body: &str) -> Option<Duration> {
    fn visit(value: &serde_json::Value) -> Option<Duration> {
        match value {
            serde_json::Value::Object(object) => {
                let is_retry = object
                    .get("@type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| {
                        matches!(
                            kind,
                            "google.rpc.RetryInfo" | "type.googleapis.com/google.rpc.RetryInfo"
                        )
                    });
                if is_retry
                    && let Some(delay) = object
                        .get("retryDelay")
                        .and_then(serde_json::Value::as_str)
                        .and_then(parse_google_duration)
                {
                    return Some(delay);
                }
                object.values().find_map(visit)
            }
            serde_json::Value::Array(values) => values.iter().find_map(visit),
            _ => None,
        }
    }

    serde_json::from_str(body)
        .ok()
        .and_then(|value| visit(&value))
}

fn parse_google_duration(raw: &str) -> Option<Duration> {
    let seconds = raw.strip_suffix('s')?;
    let (whole, fraction) = seconds.split_once('.').unwrap_or((seconds, ""));
    if whole.starts_with('-') || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let whole = whole.parse::<u64>().ok()?;
    let nanos = if fraction.is_empty() {
        0
    } else {
        let kept = &fraction[..fraction.len().min(9)];
        let value = kept.parse::<u32>().ok()?;
        value.checked_mul(10_u32.pow(9 - u32::try_from(kept.len()).ok()?))?
    };
    Some(Duration::new(whole, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answer one request on a loopback port with `status` and `body`.
    async fn serve_once_with_headers(
        status: &'static str,
        headers: &'static str,
        body: String,
    ) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0_u8; 4096];
            let _ = stream.read(&mut buf).await;
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(body.as_bytes()).await;
        });
        format!("http://{addr}/")
    }

    async fn serve_once(status: &'static str, body: String) -> String {
        serve_once_with_headers(status, "", body).await
    }

    async fn error_body_for(body: String) -> String {
        let url = serve_once("400 Bad Request", body).await;
        let resp = reqwest::Client::new().get(url).send().await.unwrap();
        provider_error_body(resp).await
    }

    async fn provider_error_for(headers: &'static str, body: String) -> LlmError {
        let url = serve_once_with_headers("429 Too Many Requests", headers, body).await;
        let resp = reqwest::Client::new().get(url).send().await.unwrap();
        provider_error(resp).await
    }

    #[tokio::test]
    async fn provider_error_body_redacts_a_secret_straddling_the_display_cut() {
        // The cut keeps `ghp_` plus 12 characters: too short for the GitHub
        // token pattern, so only scrubbing before the cut redacts it.
        let secret = format!("ghp_{}", "FAKEfake0123".repeat(3));
        let prefix = format!("{} ", "x".repeat(DISPLAY_ERROR_BYTES - 17));
        let body = format!("{prefix}{secret}");

        let shown = error_body_for(body).await;

        assert!(!shown.contains("ghp_"), "secret prefix leaked: {shown}");
        assert!(shown.starts_with(&prefix), "unrelated text was lost");
    }

    #[tokio::test]
    async fn provider_error_body_strips_terminal_control_sequences() {
        let shown = error_body_for("before\x1b]0;retitled\x07\x1b[2Jafter".to_string()).await;

        assert!(
            !shown.contains('\x1b'),
            "escape sequence survived: {shown:?}"
        );
        assert!(!shown.contains('\x07'), "bell survived: {shown:?}");
        assert!(shown.starts_with("before"));
    }

    #[tokio::test]
    async fn provider_error_body_caps_what_it_reads_and_what_it_shows() {
        let shown = error_body_for("a".repeat(MAX_PROVIDER_ERROR_BYTES - 1)).await;
        assert!(shown.len() <= DISPLAY_ERROR_BYTES + "…".len());

        let oversized = error_body_for("a".repeat(MAX_PROVIDER_ERROR_BYTES + 1)).await;
        assert!(oversized.contains("exceeded"), "unexpected: {oversized}");
    }

    #[tokio::test]
    async fn provider_error_reads_gemini_retry_info_before_the_display_cut() {
        let body = serde_json::json!({
            "error": {
                "message": "x".repeat(DISPLAY_ERROR_BYTES * 2),
                "details": [
                    {"@type": "type.googleapis.com/google.rpc.QuotaFailure"},
                    {
                        "@type": "type.googleapis.com/google.rpc.RetryInfo",
                        "retryDelay": "32993s"
                    }
                ]
            }
        })
        .to_string();

        let error = provider_error_for("", body).await;

        assert_eq!(error.retry_after(), Some(Duration::from_secs(32_993)));
        let LlmError::Provider { body, .. } = error else {
            panic!("expected provider error");
        };
        assert!(body.len() <= DISPLAY_ERROR_BYTES + "…".len());
        assert!(
            !body.contains("RetryInfo"),
            "the control must be beyond the cut"
        );
    }

    #[tokio::test]
    async fn provider_error_finds_nested_compat_retry_info_and_parses_fractional_seconds() {
        let body = serde_json::json!({
            "error": {
                "details": [[{
                    "@type": "type.googleapis.com/google.rpc.RetryInfo",
                    "retryDelay": "12.345s"
                }]]
            }
        })
        .to_string();

        let error = provider_error_for("", body).await;

        assert_eq!(error.retry_after(), Some(Duration::new(12, 345_000_000)));
    }

    #[tokio::test]
    async fn retry_after_header_precedes_the_body_and_invalid_hints_are_ignored() {
        let body = serde_json::json!({
            "details": [{
                "@type": "type.googleapis.com/google.rpc.RetryInfo",
                "retryDelay": "10s"
            }]
        })
        .to_string();
        let error = provider_error_for("Retry-After: 42\r\n", body).await;
        assert_eq!(error.retry_after(), Some(Duration::from_secs(42)));

        for raw in ["-1s", "NaNs", "1.second"] {
            let body = serde_json::json!({
                "@type": "type.googleapis.com/google.rpc.RetryInfo",
                "retryDelay": raw
            })
            .to_string();
            assert_eq!(provider_error_for("", body).await.retry_after(), None);
        }
    }
}
