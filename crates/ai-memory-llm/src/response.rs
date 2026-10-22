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
    read_provider_error_body(resp).await.0
}

/// Build the [`LlmError::Provider`] for a failed response: its status, the
/// body as [`provider_error_body`] cuts it, and the provider's retry hint.
///
/// The hint comes from a delta-seconds `Retry-After` header, or else from a
/// Google `RetryInfo` detail in the body. Gemini sends no `Retry-After` on a
/// quota 429 and puts `RetryInfo` last among the error details, past the
/// 1 KiB display cut, so the hint is read from the full body before the cut.
///
/// Example: `return Err(provider_error(resp).await);` after a non-2xx status.
pub(crate) async fn provider_error(resp: reqwest::Response) -> LlmError {
    let status = resp.status().as_u16();
    let header_hint = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(retry_after_seconds);
    let (body, body_hint) = read_provider_error_body(resp).await;
    LlmError::Provider {
        status,
        body,
        retry_after: header_hint.or(body_hint),
    }
}

/// [`provider_error`] with `context` prefixed to the body, for token
/// endpoints whose transient failures must name the step that failed.
pub(crate) async fn provider_error_in(context: &str, resp: reqwest::Response) -> LlmError {
    match provider_error(resp).await {
        LlmError::Provider {
            status,
            body,
            retry_after,
        } => LlmError::Provider {
            status,
            body: format!("{context}: {body}"),
            retry_after,
        },
        other => other,
    }
}

/// Read, scrub and cut a failed response's body, returning the cut body and
/// the `RetryInfo` delay found in the full body.
async fn read_provider_error_body(resp: reqwest::Response) -> (String, Option<Duration>) {
    match response_bytes_limited(resp, MAX_PROVIDER_ERROR_BYTES).await {
        Ok(bytes) => {
            let body = Sanitizer::builtin().scrub(&String::from_utf8_lossy(&bytes));
            let hint = google_retry_info_delay(&body);
            (truncate_with_ellipsis(&body, DISPLAY_ERROR_BYTES), hint)
        }
        Err(LlmError::Provider { body, .. }) => (body, None),
        Err(e) => (format!("<failed to read provider error body: {e}>"), None),
    }
}

/// A `Retry-After` value in its delta-seconds form (`Retry-After: 30`). The
/// HTTP-date form is not supported, matching the embedding retry loop.
pub(crate) fn retry_after_seconds(value: &reqwest::header::HeaderValue) -> Option<Duration> {
    let secs = value.to_str().ok()?.parse::<u64>().ok()?;
    Some(Duration::from_secs(secs))
}

/// `retryDelay` of the first `google.rpc.RetryInfo` detail in a Google API
/// error body (<https://google.aip.dev/193>,
/// <https://ai.google.dev/gemini-api/docs/troubleshooting>). The native
/// Gemini API answers with the error object; its OpenAI-compatible endpoint
/// wraps the same object in a one-element array.
fn google_retry_info_delay(body: &str) -> Option<Duration> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let envelope = match &value {
        serde_json::Value::Array(items) => items.first()?,
        other => other,
    };
    envelope
        .get("error")?
        .get("details")?
        .as_array()?
        .iter()
        .filter(|detail| {
            detail
                .get("@type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind.ends_with("google.rpc.RetryInfo"))
        })
        .find_map(|detail| {
            detail
                .get("retryDelay")?
                .as_str()
                .and_then(parse_protobuf_duration)
        })
}

/// A protobuf JSON `Duration`: decimal seconds with an `s` suffix, such as
/// `"32993s"` or `"1.5s"`
/// (<https://protobuf.dev/programming-guides/json/>). Negative, non-finite
/// and malformed values give `None`.
fn parse_protobuf_duration(text: &str) -> Option<Duration> {
    let secs: f64 = text.strip_suffix('s')?.parse().ok()?;
    Duration::try_from_secs_f64(secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answer one request on a loopback port with `status` and `body`.
    async fn serve_once(status: &'static str, body: String) -> String {
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
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes()).await;
            let _ = stream.write_all(body.as_bytes()).await;
        });
        format!("http://{addr}/")
    }

    async fn error_body_for(body: String) -> String {
        let url = serve_once("400 Bad Request", body).await;
        let resp = reqwest::Client::new().get(url).send().await.unwrap();
        provider_error_body(resp).await
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

    fn google_error(details: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "error": { "code": 429, "status": "RESOURCE_EXHAUSTED", "details": details }
        })
    }

    fn retry_info(delay: &str) -> serde_json::Value {
        serde_json::json!({ "@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": delay })
    }

    #[test]
    fn google_retry_info_is_read_from_the_native_object_and_the_compat_array() {
        let details = serde_json::json!([
            { "@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [] },
            retry_info("32993s"),
        ]);
        let native = google_error(details).to_string();
        let compat =
            serde_json::json!([google_error(serde_json::json!([retry_info("41s")]))]).to_string();

        assert_eq!(
            google_retry_info_delay(&native),
            Some(Duration::from_secs(32_993))
        );
        assert_eq!(
            google_retry_info_delay(&compat),
            Some(Duration::from_secs(41))
        );
    }

    #[test]
    fn google_retry_info_accepts_fractional_seconds_and_rejects_bad_values() {
        let delay = |value: &str| {
            google_retry_info_delay(
                &google_error(serde_json::json!([retry_info(value)])).to_string(),
            )
        };
        assert_eq!(delay("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(delay("-5s"), None);
        assert_eq!(delay("5"), None, "a protobuf Duration ends in `s`");
        assert_eq!(delay("soon"), None);
    }

    #[test]
    fn google_retry_info_is_absent_without_a_retry_detail() {
        let help_only = google_error(serde_json::json!([
            { "@type": "type.googleapis.com/google.rpc.Help", "links": [] }
        ]))
        .to_string();
        assert_eq!(google_retry_info_delay(&help_only), None);
        assert_eq!(google_retry_info_delay("upstream overloaded"), None);
        assert_eq!(google_retry_info_delay(r#"{"error":"rate limited"}"#), None);
    }

    #[test]
    fn retry_after_header_supports_only_delta_seconds() {
        let header = |value: &str| {
            retry_after_seconds(&reqwest::header::HeaderValue::from_str(value).unwrap())
        };
        assert_eq!(header("30"), Some(Duration::from_secs(30)));
        assert_eq!(header("1.5"), None);
        assert_eq!(header("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }
}
