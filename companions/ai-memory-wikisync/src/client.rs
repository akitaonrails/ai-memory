//! Read-only `/api/v1` client.
//!
//! Only the documented frontend endpoints are used: incremental `recent`
//! paging for the page listing (with the legacy bare-array response accepted
//! as a fallback) and single-page reads with `ETag` / `If-None-Match`
//! revalidation. No MCP, no admin routes, no writes. The bearer token is
//! only ever attached as a request header — it never reaches an error
//! message, a URL, or the state file.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use reqwest::StatusCode;
use serde::Deserialize;
use url::Url;

pub const DEFAULT_SERVER_URL: &str = "http://127.0.0.1:49374";

/// Listing anchor: everything strictly after the epoch. `updated_since` is
/// exclusive, so no page is missed.
const EPOCH: &str = "1970-01-01T00:00:00Z";
const RECENT_LIMIT: usize = 100;
/// Bound on cursor rounds so a hostile server cannot loop the client.
const MAX_LIST_ROUNDS: usize = 1_000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// One row of the pages listing (`PageSummary` in `docs/frontend-api.md`).
#[derive(Debug, Clone, Deserialize)]
pub struct PageSummary {
    pub path: String,
    pub title: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub updated_at: String,
}

/// The canonical single-page projection. Only the fields this tool
/// transports, or checks before an import, are decoded; everything else the
/// server returns is ignored rather than stored, and nothing is forged
/// locally.
#[derive(Debug, Clone, Deserialize)]
pub struct ApiPage {
    pub path: String,
    pub body_markdown: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub pinned: bool,
    /// The page's parsed frontmatter: `tags` live here, and so does any
    /// metadata an import would clear.
    #[serde(default)]
    pub frontmatter: serde_json::Value,
}

/// Result of one page read: the body on `200`, or `None` on `304 Not
/// Modified` when the supplied `If-None-Match` still matches.
#[derive(Debug)]
pub struct PageRead {
    pub page: Option<ApiPage>,
    pub etag: Option<String>,
}

pub struct ApiClient {
    http: reqwest::Client,
    base: Url,
    token: Option<String>,
}

#[derive(Deserialize)]
struct RecentIncremental {
    pages: Vec<PageSummary>,
    next_cursor: Option<String>,
}

impl ApiClient {
    pub fn new(server_url: &str, token: Option<String>) -> Result<Self> {
        let mut base =
            Url::parse(server_url).map_err(|e| anyhow!("invalid --server {server_url:?}: {e}"))?;
        if !base.username().is_empty() || base.password().is_some() {
            bail!(
                "server URL must not embed credentials; pass the token via --token \
                 or AI_MEMORY_AUTH_TOKEN instead"
            );
        }
        if base.path() != "/" && !base.path().is_empty() {
            bail!(
                "server URL must be an origin (scheme://host[:port]); base-path \
                 deployments are not supported by this companion"
            );
        }
        if base.query().is_some() || base.fragment().is_some() {
            bail!("server URL must not carry a query or fragment");
        }
        base.set_path("");
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context_build()?;
        Ok(Self { http, base, token })
    }

    fn api_url(
        &self,
        workspace: &str,
        project: &str,
        tail: &[&str],
        query: &[(&str, String)],
    ) -> Result<Url> {
        let mut url = self.base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| anyhow!("server URL cannot serve as a base"))?;
            segments
                .push("api")
                .push("v1")
                .push("workspaces")
                .push(workspace)
                .push("projects")
                .push(project);
            for segment in tail {
                segments.push(segment);
            }
        }
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in query {
                pairs.append_pair(key, value);
            }
        }
        Ok(url)
    }

    async fn send(
        &self,
        url: &Url,
        if_none_match: Option<&str>,
        not_found_hint: &str,
    ) -> Result<reqwest::Response> {
        let mut request = self
            .http
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        if let Some(etag) = if_none_match {
            request = request.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = request
            .send()
            .await
            .map_err(|e| anyhow!("request to {} failed: {e}", url.as_str()))?;
        check_status(response, url, not_found_hint)
    }

    /// Read one full page, or `None` when the server has no such page. Used
    /// right before an import, where absence is an answer, not an error.
    pub async fn read_page_if_exists(
        &self,
        workspace: &str,
        project: &str,
        path: &str,
    ) -> Result<Option<ApiPage>> {
        let tail: Vec<&str> = path.split('/').collect();
        let url = self.build_page_url(workspace, project, &tail)?;
        let mut request = self
            .http
            .get(url.clone())
            .header(reqwest::header::ACCEPT, "application/json");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| anyhow!("request to {} failed: {e}", url.as_str()))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response = check_status(response, &url, "")?;
        let page: ApiPage = response.json().await.map_err(|e| {
            anyhow!(
                "page read from {} returned malformed JSON: {e}",
                url.as_str()
            )
        })?;
        if page.path != path {
            bail!(
                "server answered a read of {path:?} with page {:?}; refusing the mismatch",
                page.path
            );
        }
        Ok(Some(page))
    }

    /// List the project's latest pages via incremental `recent` paging from
    /// the epoch. Incremental results already exclude superseded and expired
    /// pages, which is exactly the latest-only projection an export wants.
    /// A server without incremental support answers the legacy bare array;
    /// that is accepted as the full listing.
    pub async fn list_pages(&self, workspace: &str, project: &str) -> Result<Vec<PageSummary>> {
        let mut all: Vec<PageSummary> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_ROUNDS {
            let mut query = vec![
                ("updated_since", EPOCH.to_string()),
                ("limit", RECENT_LIMIT.to_string()),
            ];
            if let Some(value) = &cursor {
                query.push(("cursor", value.clone()));
            }
            let url = self.api_url(workspace, project, &["recent"], &query)?;
            let response = self.send(&url, None, "check --workspace/--project").await?;
            let bytes = response
                .bytes()
                .await
                .map_err(|e| anyhow!("reading {} failed: {e}", url.as_str()))?;
            let first = bytes
                .iter()
                .find(|byte| !byte.is_ascii_whitespace())
                .copied()
                .unwrap_or(b'{');
            if first == b'[' {
                let pages: Vec<PageSummary> = serde_json::from_slice(&bytes).map_err(|e| {
                    anyhow!(
                        "legacy page listing from {} is malformed: {e}",
                        url.as_str()
                    )
                })?;
                all.extend(pages);
                return Ok(all);
            }
            let page: RecentIncremental = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow!("page listing from {} is malformed: {e}", url.as_str()))?;
            all.extend(page.pages);
            match page.next_cursor {
                Some(next) => {
                    if Some(&next) == cursor.as_ref() {
                        bail!(
                            "server repeated the same listing cursor; aborting to avoid \
                             an infinite loop"
                        );
                    }
                    cursor = Some(next);
                }
                None => return Ok(all),
            }
        }
        bail!("page listing did not finish within {MAX_LIST_ROUNDS} cursor rounds; aborting");
    }

    /// Read one full page, revalidating a stored ETag with `If-None-Match`.
    /// A `404` here means the listing and the read raced (the page was
    /// deleted or expired mid-run), which fails loudly instead of silently
    /// skipping.
    pub async fn read_page(
        &self,
        workspace: &str,
        project: &str,
        path: &str,
        if_none_match: Option<&str>,
    ) -> Result<PageRead> {
        // Each path segment stays its own path component so the wiki path's
        // '/' separators are preserved in the request URL.
        let tail: Vec<&str> = path.split('/').collect();
        let url = self.build_page_url(workspace, project, &tail)?;
        let response = self
            .send(
                &url,
                if_none_match,
                &format!("page {path:?} was not found (deleted or expired mid-run?)"),
            )
            .await?;
        let etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .or_else(|| if_none_match.map(str::to_owned));
        if response.status() == StatusCode::NOT_MODIFIED {
            return Ok(PageRead { page: None, etag });
        }
        let page: ApiPage = response.json().await.map_err(|e| {
            anyhow!(
                "page read from {} returned malformed JSON: {e}",
                url.as_str()
            )
        })?;
        if page.path != path {
            bail!(
                "server answered a read of {path:?} with page {:?}; refusing the mismatch",
                page.path
            );
        }
        Ok(PageRead {
            page: Some(page),
            etag,
        })
    }

    fn build_page_url(&self, workspace: &str, project: &str, tail: &[&str]) -> Result<Url> {
        let mut url = self.base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| anyhow!("server URL cannot serve as a base"))?;
            segments
                .push("api")
                .push("v1")
                .push("workspaces")
                .push(workspace)
                .push("projects")
                .push(project)
                .push("pages");
            for segment in tail {
                segments.push(segment);
            }
        }
        Ok(url)
    }
}

fn check_status(
    response: reqwest::Response,
    url: &Url,
    not_found_hint: &str,
) -> Result<reqwest::Response> {
    match response.status() {
        StatusCode::OK | StatusCode::NOT_MODIFIED => Ok(response),
        StatusCode::UNAUTHORIZED => bail!(
            "GET {} returned 401 Unauthorized — the server requires a bearer token \
             (pass --token or set AI_MEMORY_AUTH_TOKEN)",
            url.as_str()
        ),
        StatusCode::FORBIDDEN => bail!(
            "GET {} returned 403 Forbidden — the token was rejected, the caller lacks \
             access to this scope, or the server's host allowlist refused the request",
            url.as_str()
        ),
        StatusCode::NOT_FOUND => bail!(
            "GET {} returned 404 Not Found — {not_found_hint}",
            url.as_str()
        ),
        status => bail!("GET {} returned unexpected status {status}", url.as_str()),
    }
}

/// Small helper so the client builder error reads like the others.
trait ContextBuild {
    fn context_build(self) -> Result<reqwest::Client>;
}

impl ContextBuild for Result<reqwest::Client, reqwest::Error> {
    fn context_build(self) -> Result<reqwest::Client> {
        self.map_err(|e| anyhow!("cannot build HTTP client: {e}"))
    }
}
