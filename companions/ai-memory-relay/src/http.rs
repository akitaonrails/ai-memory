//! The only network surface: `POST <server>/hook/batch`.
//!
//! Per-item URLs are always constructed here from the bound destination and the
//! queue's own fields. An input file can never steer a request anywhere.

use std::io::Read;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use url::Url;

use crate::queue::{Binding, PendingItem};

/// Bearer material. Never stored on disk, never printed, never in a Debug line.
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

impl Token {
    /// Read `AI_MEMORY_AUTH_TOKEN` from the environment of *this* flush.
    ///
    /// The value is never echoed, not even when it is rejected.
    pub fn from_env() -> Result<Option<Self>> {
        let Ok(raw) = std::env::var("AI_MEMORY_AUTH_TOKEN") else {
            return Ok(None);
        };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        if trimmed.chars().any(|c| c.is_control()) {
            bail!("AI_MEMORY_AUTH_TOKEN contains control characters; refusing to build a request");
        }
        Ok(Some(Self(trimmed.to_owned())))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

/// Normalize and harden `--server-url`.
///
/// http/https only, a host, no userinfo, no query, no fragment. No host
/// allowlist: a remote HTTPS server is a legitimate deployment.
pub fn normalize_server_url(raw: &str) -> Result<String> {
    let url = Url::parse(raw).context("invalid --server-url")?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("--server-url must be http or https, got {:?}", url.scheme());
    }
    if !url.username().is_empty() || url.password().is_some() {
        bail!("--server-url must not carry userinfo; pass credentials in AI_MEMORY_AUTH_TOKEN");
    }
    if url.query().is_some() {
        bail!("--server-url must not carry a query string");
    }
    if url.fragment().is_some() {
        bail!("--server-url must not carry a fragment");
    }
    let host = url.host_str().context("--server-url needs a host")?;
    let authority = match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    };
    let path = url.path().trim_end_matches('/');
    Ok(format!("{}://{}{}", url.scheme(), authority, path))
}

/// The `{url, body}` pair the batch endpoint expects.
#[derive(Debug, serde::Serialize)]
pub struct BatchItem {
    pub url: String,
    pub body: serde_json::Value,
}

/// Build one item's hook URL from the binding plus the queued event.
///
/// `extension` carries the producer namespace, `source_event` repeats the
/// canonical event explicitly (both are required to preserve provenance), and
/// `ingest_key` is the stable key persisted with the event at enqueue time.
pub fn item_url(base: &str, binding: &Binding, item: &PendingItem) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("event", &item.event)
        .append_pair("agent", &item.agent)
        .append_pair("workspace", &binding.workspace)
        .append_pair("project", &binding.project)
        .append_pair("extension", &binding.producer)
        .append_pair("source_event", &item.event)
        .append_pair("ingest_key", &item.ingest_key)
        .finish();
    format!("{base}/hook?{query}")
}

/// Largest ack body the relay will read. A 256-index ack is under 2 KiB; 64 KiB
/// leaves room for future fields while refusing to stream an unbounded response
/// from a server that is not behaving like ai-memory.
pub const MAX_ACK_BYTES: usize = 64 * 1024;

/// One completed HTTP exchange. The body is bounded and never logged.
pub struct Delivery {
    pub status: u16,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for Delivery {
    // Length only: an ack body is server output, and output never becomes a log
    // line here by accident.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Delivery")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Blocking batch sender.
pub struct Sender {
    client: reqwest::blocking::Client,
    base: String,
    token: Option<Token>,
}

impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sender")
            .field("base", &self.base)
            .field("token", &self.token)
            .finish()
    }
}

impl Sender {
    /// Build the client. Redirects are disabled: a 3xx from the ingestion
    /// endpoint must never resend the bearer header, or the batch, elsewhere.
    /// The read timeout is per request, because a batch's budget scales with the
    /// number of items it carries.
    pub fn new(base: String, token: Option<Token>, connect_timeout: Duration) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .build()
            .context("build HTTP client")?;
        Ok(Self {
            client,
            base,
            token,
        })
    }

    /// POST one batch under an explicit timeout.
    ///
    /// Transport failures carry a short class label only: never the request, the
    /// response, or the bearer header.
    pub fn post_batch(&self, items: &[BatchItem], timeout: Duration) -> Result<Delivery> {
        let mut request = self
            .client
            .post(format!("{}/hook/batch", self.base))
            .timeout(timeout)
            .json(items);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose());
        }
        let response = request.send().map_err(|e| {
            anyhow::anyhow!("hook batch transport failure: {}", transport_class(&e))
        })?;
        let status = response.status().as_u16();
        // Bounded read: `bytes()` would accept whatever the peer sends.
        let mut body = Vec::new();
        let mut limited = response.take(MAX_ACK_BYTES as u64 + 1);
        if limited.read_to_end(&mut body).is_err() {
            body.clear();
        }
        if body.len() > MAX_ACK_BYTES {
            bail!(
                "hook batch ack exceeded {MAX_ACK_BYTES} bytes; refusing to parse it \
                 (the batch stays pending)"
            );
        }
        Ok(Delivery { status, body })
    }
}

/// A short, content-free label for a transport error.
fn transport_class(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_redirect() {
        "redirect refused"
    } else if error.is_request() {
        "request"
    } else {
        "io"
    }
}
