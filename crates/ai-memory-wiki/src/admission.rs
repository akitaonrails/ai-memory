//! HTTP admission webhook chain — engine's *only* extension point for
//! enriching/validating pages just before persistence.
//!
//! ## Design (OCP)
//!
//! The engine ships ONE generic primitive: a chain of HTTP webhooks invoked
//! synchronously inside [`Wiki::write_page`](crate::Wiki::write_page) AFTER
//! the [`Markdown`](crate::Markdown) is built but BEFORE [`crate::emit`] +
//! atomic write. Each webhook receives the page (path + frontmatter + body)
//! and an [`AdmissionContext`] (actor identity + workspace/project scope),
//! and may:
//!
//! - Return `200 OK` with a mutated `{ page }` → engine substitutes
//!   `frontmatter` and/or `body` before persistence.
//! - Return `204 No Content` → no mutation, chain continues.
//! - Return `4xx/5xx` → behaviour governed by the webhook's
//!   [`FailurePolicy`] (`Ignore` = log+skip; `Reject` = abort the write).
//!
//! Each new domain extension (`contributors`, `cost-tracker`, `git-mirror`,
//! `review-marker`, …) becomes a NEW external HTTP service — engine never
//! grows for new fields/behaviours.
//!
//! ## Loop prevention
//!
//! Callers may set [`AdmissionContext::skip_webhooks`] from the
//! `X-Memory-Skip-Admission-Chain` request header — that lets a webhook
//! that calls back into the engine (e.g. to write a derived page) opt out
//! of being re-invoked on the recursive write.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::PagePath;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::error::WikiError;
use crate::{Markdown, WikiResult};

/// Why a `reject`-policy webhook stopped [`AdmissionChain::run`]. It travels
/// as the source of the returned `WikiError::Io`, whose message is this
/// one's, so callers that only log or display the error see no change.
#[derive(Debug)]
pub(crate) struct AdmissionFailure {
    /// The webhook could not give a verdict (transport error, timeout, 5xx),
    /// as opposed to answering with a refusal.
    unavailable: bool,
    message: String,
}

impl std::fmt::Display for AdmissionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AdmissionFailure {}

impl AdmissionFailure {
    fn into_error(unavailable: bool, message: String) -> WikiError {
        WikiError::Io(std::io::Error::other(Self {
            unavailable,
            message,
        }))
    }
}

/// `true` when `err` came from [`AdmissionChain::run`] because a webhook was
/// unreachable, timed out or answered 5xx, rather than refusing the page.
pub(crate) fn is_admission_unavailable(err: &WikiError) -> bool {
    match err {
        WikiError::Io(io) => io
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<AdmissionFailure>())
            .is_some_and(|failure| failure.unavailable),
        _ => false,
    }
}

/// Lifecycle operation that triggered the chain. Webhooks subscribe via
/// [`WebhookConfig::events`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionOp {
    /// A `Wiki::write_page` call (most writes — MCP `memory_write_page`,
    /// CLI `write-page`, admin endpoint).
    #[default]
    WritePage,
    /// An LLM consolidation write (consolidator/lint compile observations
    /// into a durable page).
    Consolidate,
    /// A single page is being deleted (`Wiki::delete_page`). Carries the
    /// page path; no body. Lets a mirror `git rm` the file.
    Delete,
    /// A hook event ledger file (`log-YYYY-MM.md`) is being deleted by the
    /// opt-in ledger retention pass of the forget sweep
    /// (`Wiki::delete_event_ledger`). Carries the ledger file name in the
    /// page path; no body, and no `pages` row to touch (ledgers are files,
    /// not pages). Fired **before** the file is removed, so a `reject`-policy
    /// webhook keeps it; a mirror subscribed to it can `git rm` the file.
    DeleteEventLedger,
    /// A whole project is being purged (`Wiki::purge_project` →
    /// `remove_dir_all`). Carries the project (ctx), no page path. Lets a
    /// mirror remove the project's directory.
    PurgeProject,
    /// One session is being purged (`purge_session`): its rows, the handoffs
    /// it authored, and its `sessions/<id>.md` page and every version of it.
    /// Carries the workspace/project; no page path, since the operation is
    /// driven by session id rather than by path. Lets a mirror drop the
    /// session page file, and lets a scope-guard webhook refuse the delete
    /// before any row is touched.
    PurgeSession,
    /// A whole workspace is being purged. Carries the workspace (ctx), no page
    /// path, and an empty project. Lets a mirror remove the workspace tree.
    PurgeWorkspace,
    /// A whole project is being moved between workspaces without changing its
    /// project id. Carries source names in `workspace`/`project` and
    /// destination names in `destination_workspace`/`destination_project`.
    MoveProject,
    /// One session (its rows and its `sessions/<id>.md` page) is being moved
    /// to another project. Carries the source names in `workspace`/`project`
    /// and the destination names in `destination_workspace`/
    /// `destination_project`; no page path. Lets a mirror move or drop the
    /// session page file.
    MoveSession,
    /// A handoff is being created. Carries the workspace/project; no page path,
    /// since handoffs live in their own table rather than the wiki tree.
    ///
    /// The handoff lifecycle never touched `Wiki::write_page`, so an admission
    /// webhook could not see — let alone refuse — any of it. A deployment that
    /// authorizes writes per operator had a blind spot exactly on the rows that
    /// carry prompt-derived text between operators.
    HandoffBegin,
    /// A pending handoff is being consumed.
    HandoffAccept,
    /// A pending handoff is being discarded.
    HandoffCancel,
    /// A cross-project message is being sent into a recipient project's inbox
    /// (V64). Carries the RECIPIENT workspace/project (the crossing point); no
    /// page path, since messages live in their own table. Lets a per-operator
    /// admission webhook see and refuse prompt-derived text crossing projects.
    MessageSend,
    /// A cross-project message is being popped (consumed) from an inbox.
    MessagePop,
    /// Pending cross-project messages are being cancelled by their sender.
    MessageCancel,
}

impl AdmissionOp {
    /// String value for the `X-Memory-Op` request header sent to webhooks.
    #[must_use]
    pub fn as_header_value(&self) -> &'static str {
        match self {
            AdmissionOp::WritePage => "write_page",
            AdmissionOp::Consolidate => "consolidate",
            AdmissionOp::Delete => "delete",
            AdmissionOp::DeleteEventLedger => "delete_event_ledger",
            AdmissionOp::PurgeProject => "purge_project",
            AdmissionOp::PurgeSession => "purge_session",
            AdmissionOp::PurgeWorkspace => "purge_workspace",
            AdmissionOp::MoveProject => "move_project",
            AdmissionOp::MoveSession => "move_session",
            AdmissionOp::HandoffBegin => "handoff_begin",
            AdmissionOp::HandoffAccept => "handoff_accept",
            AdmissionOp::HandoffCancel => "handoff_cancel",
            AdmissionOp::MessageSend => "message_send",
            AdmissionOp::MessagePop => "message_pop",
            AdmissionOp::MessageCancel => "message_cancel",
        }
    }
}

/// What the engine does when a webhook returns 4xx/5xx or fails to respond.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailurePolicy {
    /// Log a warning and continue with the unmutated page. Safer default —
    /// page writes never blocked by buggy/down webhooks.
    #[default]
    Ignore,
    /// Abort the write with an error. Use for safety-critical webhooks
    /// (e.g. a `validate-no-secrets` enforcer).
    Reject,
}

/// serde `skip_serializing_if` helper for `bool` fields that default
/// to `false` — skip the field entirely when it carries no information
/// so existing webhook consumers don't see an unknown extra key.
fn is_false(b: &bool) -> bool {
    !b
}

fn default_timeout_ms() -> u64 {
    2_000
}

fn default_blocking() -> bool {
    true
}

/// One webhook entry in the chain. Loaded from operator config
/// (`[[admission_webhooks]]`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    /// Stable identifier (used by loop-prevention skip lists + log fields).
    pub name: String,
    /// HTTP endpoint to POST the payload to.
    pub url: String,
    /// Per-request timeout in milliseconds. Default 2000.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// What to do on webhook failure. Default [`FailurePolicy::Ignore`].
    #[serde(default)]
    pub failure_policy: FailurePolicy,
    /// Subset of [`AdmissionOp`] this webhook subscribes to. If empty,
    /// the webhook is effectively disabled (it'll never fire).
    pub events: Vec<AdmissionOp>,
    /// When `true` (default), the webhook runs **synchronously** inside the
    /// write path — it can mutate the page (`run`) and a `Reject` failure
    /// aborts the write. When `false`, it's dispatched **fire-and-forget**
    /// AFTER the page has landed: the engine doesn't wait for it and ignores
    /// its response (so it can't mutate or reject). Use `false` for pure
    /// observers/mirrors (e.g. `git-mirror`) so a slow/down backup never adds
    /// latency to writes.
    #[serde(default = "default_blocking")]
    pub blocking: bool,
    /// Projects this webhook is told about, each `workspace/project`. Empty
    /// (the default) means every project, as before the key existed. A
    /// workspace-wide event (empty project) matches an entry by its workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
    /// Extra request headers whose values live in files, so an endpoint that
    /// wants `Authorization: Bearer …` is reachable without a relay and without
    /// a secret in `config.toml`. Each file is read once, when the chain is
    /// built; the value never reaches a log, an error or `Debug`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<WebhookHeader>,
}

/// One extra header for a webhook. The config holds only the file path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookHeader {
    /// Header name, e.g. `Authorization`.
    pub name: String,
    /// File holding the value, trimmed of surrounding whitespace. A relative
    /// path is resolved against the data dir when the config is loaded.
    pub value_file: PathBuf,
}

/// Headers the engine sets itself (or the HTTP client derives); a configured
/// header must not replace them.
const RESERVED_WEBHOOK_HEADERS: [&str; 4] =
    ["content-type", "content-length", "host", "x-memory-op"];

/// A webhook with its `projects` parsed and its header files read: what the
/// chain actually dispatches. Built once by [`AdmissionChain::new`].
#[derive(Clone)]
struct ChainHook {
    config: WebhookConfig,
    /// `(workspace, project)` pairs from [`WebhookConfig::projects`].
    projects: Vec<(String, String)>,
    /// Resolved values, each marked sensitive so `Debug` shows no value.
    headers: reqwest::header::HeaderMap,
}

impl std::ops::Deref for ChainHook {
    type Target = WebhookConfig;

    fn deref(&self) -> &WebhookConfig {
        &self.config
    }
}

impl std::fmt::Debug for ChainHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainHook")
            .field("config", &self.config)
            .field("header_names", &self.headers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl ChainHook {
    fn resolve(config: WebhookConfig) -> WikiResult<Self> {
        let invalid = |detail: String| {
            WikiError::Io(std::io::Error::other(format!(
                "admission webhook '{}' {detail}",
                config.name
            )))
        };
        let mut projects = Vec::with_capacity(config.projects.len());
        for entry in &config.projects {
            match entry.split_once('/') {
                Some((ws, proj)) if !ws.is_empty() && !proj.is_empty() && !proj.contains('/') => {
                    projects.push((ws.to_string(), proj.to_string()));
                }
                _ => {
                    return Err(invalid(format!(
                        "has an invalid projects entry '{entry}'; expected 'workspace/project'"
                    )));
                }
            }
        }
        let mut headers = reqwest::header::HeaderMap::new();
        for header in &config.headers {
            let name = reqwest::header::HeaderName::from_bytes(header.name.as_bytes())
                .map_err(|_| invalid(format!("has an invalid header name '{}'", header.name)))?;
            if RESERVED_WEBHOOK_HEADERS.contains(&name.as_str()) {
                return Err(invalid(format!(
                    "may not set the reserved header '{}'",
                    header.name
                )));
            }
            let path = header.value_file.display();
            // Error text names the file, never its contents: the value is a
            // secret, and an io::Error never quotes what it failed to read.
            let raw = std::fs::read_to_string(&header.value_file).map_err(|e| {
                invalid(format!(
                    "cannot read the value of header '{}' from {path}: {}",
                    header.name,
                    e.kind()
                ))
            })?;
            let value = raw.trim();
            if value.is_empty() {
                return Err(invalid(format!(
                    "header '{}' value file {path} is empty",
                    header.name
                )));
            }
            let mut value = reqwest::header::HeaderValue::from_str(value).map_err(|_| {
                invalid(format!(
                    "header '{}' value in {path} is not a valid header value",
                    header.name
                ))
            })?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(Self {
            config,
            projects,
            headers,
        })
    }

    /// `true` when this webhook is told about `ctx`'s project: always without
    /// a filter, else by an exact `workspace/project` match, or by workspace
    /// for a workspace-wide event.
    fn watches(&self, ctx: &AdmissionContext) -> bool {
        self.projects.is_empty()
            || self.projects.iter().any(|(ws, proj)| {
                *ws == ctx.workspace && (ctx.project.is_empty() || *proj == ctx.project)
            })
    }
}

/// Per-write context passed to each webhook. Cheap to construct
/// (mostly references at the request layer, owned here for serialisation).
#[derive(Debug, Clone, Default, Serialize)]
pub struct AdmissionContext {
    /// Workspace name the write belongs to. Resolved automatically by
    /// [`crate::Wiki::write_page`] from `workspace_id` when the wiki has
    /// been built with [`crate::Wiki::with_store_reader`]; left empty
    /// otherwise. Webhooks rely on this to address pages by the same
    /// human-readable name the engine and UI use, instead of
    /// re-implementing UUID→name lookup or falling back to a placeholder.
    #[serde(default)]
    pub workspace: String,
    /// Project name the write belongs to. Resolution mirrors `workspace`
    /// (auto-filled from `project_id` when the wiki has a store reader).
    #[serde(default)]
    pub project: String,
    /// Destination workspace name for project-level moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_workspace: Option<String>,
    /// Destination project name for project-level moves.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination_project: Option<String>,
    /// Identity of the actor that triggered the write. The canonical
    /// multi-user [`ai_memory_core::ActorContext`] (since the v0.8 merge);
    /// `Wiki::write_page` fills it from `WritePageRequest::actor` so the
    /// webhook payload and the on-disk `last_modified_by` block share one
    /// identity source.
    pub actor: ai_memory_core::ActorContext,
    /// Which lifecycle op fired the chain.
    pub op: AdmissionOp,
    /// Set to `true` when the underlying op completed its durable DB
    /// work but failed to complete a follow-on filesystem step (e.g.
    /// `remove_project_dir` returned an io::Error during a purge).
    /// The DB-side mutation is irrevocable; this flag lets mirrors that
    /// track filesystem reality (a git-push mirror) refuse to drop their
    /// own copy, while mirrors that track DB intent can ignore it.
    /// Always `false` for write-page chains; only purge/move-source
    /// callers set it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial_failure: bool,
    /// `message_send` only: the queued message's id, so an observer (a bot
    /// woken by this webhook) can pop exactly that message. Never the text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// `message_send` only: the harness the message is addressed to; absent
    /// for shared project mail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_agent: Option<ai_memory_core::AgentKind>,
    /// `message_send` only: the sender's workspace name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_workspace: Option<String>,
    /// `message_send` only: the sender's project name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_project: Option<String>,
    /// Names from `X-Memory-Skip-Admission-Chain` (CSV at the request layer);
    /// matched against [`WebhookConfig::name`] to short-circuit re-entrant writes.
    #[serde(default, skip_serializing)]
    pub skip_webhooks: Vec<String>,
}

/// Wire format sent to each webhook (one POST per webhook per write).
#[derive(Serialize)]
struct WebhookRequestBody<'a> {
    page: WebhookPagePayload<'a>,
    ctx: &'a AdmissionContext,
}

#[derive(Serialize)]
struct WebhookPagePayload<'a> {
    path: &'a str,
    frontmatter: &'a serde_json::Value,
    body: &'a str,
}

/// Wire format expected back from each webhook on `200 OK`. Both inner
/// fields are optional: the webhook may mutate only frontmatter, only body,
/// or both. Anything missing means "leave that field unchanged".
#[derive(Deserialize, Debug, Default)]
struct WebhookResponseBody {
    #[serde(default)]
    page: Option<WebhookResponsePage>,
}

#[derive(Deserialize, Debug, Default)]
struct WebhookResponsePage {
    #[serde(default)]
    frontmatter: Option<serde_json::Value>,
    #[serde(default)]
    body: Option<String>,
}

/// Sanity cap on the number of webhooks per chain. The chain runs
/// sequentially inside `Wiki::write_page`, so each entry adds
/// `timeout_ms` to worst-case write latency — beyond this many entries
/// the operator almost certainly mis-templated the config (e.g. helm
/// loop) and would be better served by an out-of-band fan-out service.
pub const MAX_ADMISSION_WEBHOOKS: usize = 16;

/// Maximum timeout a single webhook request may consume. Operators can set a
/// lower value per hook; larger values are clamped so misconfiguration cannot
/// pin write-path or async admission work indefinitely.
pub const MAX_WEBHOOK_TIMEOUT_MS: u64 = 30_000;

/// Maximum fire-and-forget webhook requests in flight for one process. Beyond
/// this cap the request is dropped with a log line; the durable operation has
/// already completed, so backpressure here never stalls the caller.
///
/// Every spawned webhook shares the permits, not just the `blocking = false`
/// ones: [`AdmissionChain::dispatch_notify_observers`] spawns any subscriber
/// that cannot refuse the op, which on the handoff lifecycle ops includes a
/// `blocking = true, failure_policy = ignore` hook. So a hook an operator marked
/// blocking can be dropped under load — it is an observer there whatever its
/// `blocking` flag says, since only `reject` policy is awaited. Dropping one is
/// logged at ERROR precisely because that configuration reads as "must run".
pub const MAX_ASYNC_ADMISSION_IN_FLIGHT: usize = 256;

/// Maximum bytes read from a single webhook response body. Webhooks
/// only need to return the mutated `page` envelope; multi-megabyte
/// responses are pathological (faulty webhook or hostile peer) and
/// would force the engine to buffer them mid-write. Anything beyond
/// this is treated as malformed and the response is ignored — same
/// safety profile as a 4xx under `FailurePolicy::Ignore`.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// The chain. Cloneable (cheap — `Arc<Vec<…>>` + `reqwest::Client`).
#[derive(Clone, Debug)]
pub struct AdmissionChain {
    webhooks: Arc<Vec<ChainHook>>,
    client: reqwest::Client,
    async_semaphore: Arc<tokio::sync::Semaphore>,
}

impl AdmissionChain {
    /// Build a chain from operator-provided webhook configs. Constructs a
    /// shared `reqwest::Client` for connection reuse.
    ///
    /// # Errors
    /// - [`WikiError::Io`] if the HTTP client cannot be built.
    /// - [`WikiError::Io`] if `webhooks.len()` exceeds
    ///   [`MAX_ADMISSION_WEBHOOKS`] (see the constant docs).
    pub fn new(webhooks: Vec<WebhookConfig>) -> WikiResult<Self> {
        if webhooks.len() > MAX_ADMISSION_WEBHOOKS {
            return Err(WikiError::Io(std::io::Error::other(format!(
                "admission chain capped at {MAX_ADMISSION_WEBHOOKS} webhooks, got {}",
                webhooks.len()
            ))));
        }

        // Validate each webhook URL scheme before any HTTP client is built.
        // Only http and https are accepted; file://, ftp://, custom schemes,
        // or bare hostnames would otherwise let an attacker that controls the
        // operator config redirect writes to local files or unexpected
        // network targets.
        for hook in &webhooks {
            if !is_supported_webhook_url(&hook.url) {
                return Err(WikiError::Io(std::io::Error::other(format!(
                    "admission webhook '{}' has unsupported URL scheme '{}'; \
                     only http:// and https:// are allowed",
                    hook.name, hook.url,
                ))));
            }
        }

        let webhooks = webhooks
            .into_iter()
            .map(ChainHook::resolve)
            .collect::<WikiResult<Vec<_>>>()?;

        let client = reqwest::Client::builder()
            .redirect(same_origin_redirects())
            .build()
            .map_err(|e| {
                WikiError::Io(std::io::Error::other(format!("admission http client: {e}")))
            })?;
        Ok(Self {
            webhooks: Arc::new(webhooks),
            client,
            async_semaphore: Arc::new(tokio::sync::Semaphore::new(MAX_ASYNC_ADMISSION_IN_FLIGHT)),
        })
    }

    /// `true` if no webhooks are configured (caller can skip the round-trip).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.webhooks.is_empty()
    }

    /// Run the chain against `markdown` (mutating `frontmatter`/`body` in
    /// place). Webhooks are invoked sequentially in config order; each sees
    /// the output of the previous one.
    ///
    /// # Errors
    /// Returns [`WikiError`] only when a webhook with
    /// [`FailurePolicy::Reject`] fails — otherwise errors are logged and
    /// skipped (`FailurePolicy::Ignore`).
    pub async fn run(
        &self,
        page_path: &PagePath,
        markdown: &mut Markdown,
        ctx: &AdmissionContext,
    ) -> WikiResult<()> {
        for hook in self.webhooks.iter() {
            // Caller opt-out (loop prevention), op subscription and project filter.
            if !self.subscribed(hook, ctx) {
                continue;
            }
            // Non-blocking webhooks are fired fire-and-forget after the write
            // (see `dispatch_async`); they never run in the synchronous,
            // mutation-capable path.
            if !hook.blocking {
                continue;
            }

            let payload = WebhookRequestBody {
                page: WebhookPagePayload {
                    path: page_path.as_str(),
                    frontmatter: &markdown.frontmatter,
                    body: &markdown.body,
                },
                ctx,
            };

            let result = self
                .client
                .post(&hook.url)
                .headers(hook.headers.clone())
                .header("X-Memory-Op", ctx.op.as_header_value())
                .timeout(webhook_timeout(hook.timeout_ms))
                .json(&payload)
                .send()
                .await;

            match result {
                Ok(resp) if resp.status().is_success() => {
                    if resp.status().as_u16() == 204 {
                        tracing::debug!(webhook = %hook.name, "admission no-op (204)");
                        continue;
                    }
                    let bytes = match read_response_limited(resp).await {
                        Ok(Some(b)) => b,
                        Ok(None) => {
                            tracing::warn!(
                                webhook = %hook.name,
                                cap = MAX_RESPONSE_BYTES,
                                "admission response exceeds cap; treating as no-op",
                            );
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(
                                webhook = %hook.name,
                                error = %e,
                                "admission response read failed; treating as no-op",
                            );
                            continue;
                        }
                    };
                    match serde_json::from_slice::<WebhookResponseBody>(&bytes) {
                        Ok(parsed) => {
                            if let Some(page) = parsed.page {
                                if let Some(new_fm) = page.frontmatter {
                                    markdown.frontmatter = new_fm;
                                }
                                if let Some(new_body) = page.body {
                                    markdown.body = new_body;
                                }
                                tracing::debug!(webhook = %hook.name, "admission mutation applied");
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                webhook = %hook.name,
                                error = %e,
                                "admission response not JSON; treating as no-op",
                            );
                        }
                    }
                }
                Ok(resp) => {
                    let status = resp.status();
                    let body_txt = response_text_limited(resp).await;
                    let err_msg = format!(
                        "admission webhook {} status {}: {}",
                        hook.name, status, body_txt
                    );
                    tracing::warn!(webhook = %hook.name, status = %status, body = %body_txt, "admission error response");
                    if matches!(hook.failure_policy, FailurePolicy::Reject) {
                        let unavailable = status.is_server_error()
                            || matches!(
                                status,
                                reqwest::StatusCode::REQUEST_TIMEOUT
                                    | reqwest::StatusCode::TOO_MANY_REQUESTS
                            );
                        return Err(AdmissionFailure::into_error(unavailable, err_msg));
                    }
                }
                Err(e) => {
                    let detail = format_request_error(&e);
                    tracing::warn!(
                        webhook = %hook.name,
                        error = %detail,
                        "admission request failed",
                    );
                    if matches!(hook.failure_policy, FailurePolicy::Reject) {
                        return Err(AdmissionFailure::into_error(
                            true,
                            format!("admission webhook {} request failed: {detail}", hook.name),
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// Notify webhooks of a delete / purge / project move. Unlike
    /// [`Self::run`], there is no body to send or mutate — the webhook acts on
    /// `ctx.op` + the (optional) page path, e.g. a mirror `git rm`s the file,
    /// removes a project directory, or renames it. Honours the same skip-list,
    /// op-subscription, timeout, and failure policy.
    ///
    /// # Errors
    /// Returns an error only when a `Reject`-policy webhook fails.
    pub async fn notify(&self, page_path: Option<&str>, ctx: &AdmissionContext) -> WikiResult<()> {
        for hook in self.webhooks.iter() {
            if !self.subscribed(hook, ctx) {
                continue;
            }
            if !hook.blocking {
                continue;
            }
            self.notify_once(hook, page_path, ctx).await?;
        }
        Ok(())
    }

    /// Run only the webhooks that can actually refuse `ctx.op` — the blocking
    /// ones the operator set to [`FailurePolicy::Reject`].
    ///
    /// For a caller whose own latency is bounded (the session-start hook path),
    /// an `Ignore` webhook is dead weight in front of the operation: its answer
    /// cannot change the outcome, yet waiting for it spends the caller's whole
    /// budget, and cutting it short mid-chain would abandon an operation a
    /// webhook earlier in the chain has already been told succeeded. Those
    /// hooks belong on [`Self::dispatch_notify_observers`], AFTER the operation
    /// has actually landed.
    ///
    /// # Errors
    /// Returns an error only when a `Reject`-policy webhook fails.
    pub async fn authorize(
        &self,
        page_path: Option<&str>,
        ctx: &AdmissionContext,
    ) -> WikiResult<()> {
        for hook in self.webhooks.iter() {
            if !self.subscribed(hook, ctx) {
                continue;
            }
            if !decides(hook) {
                continue;
            }
            self.notify_once(hook, page_path, ctx).await?;
        }
        Ok(())
    }

    /// Fire-and-forget the notify-style webhooks [`Self::authorize`] did not
    /// run: observers, which have nothing to decide. Call it once the operation
    /// is durable, so no webhook is ever told about work the engine then
    /// abandons.
    pub fn dispatch_notify_observers(&self, page_path: Option<&str>, ctx: &AdmissionContext) {
        let empty_frontmatter = serde_json::Value::Object(serde_json::Map::new());
        for hook in self.webhooks.iter() {
            if !self.subscribed(hook, ctx) {
                continue;
            }
            if decides(hook) {
                continue;
            }
            self.spawn_webhook(hook, page_path, &empty_frontmatter, "", ctx);
        }
    }

    /// Shared filter: caller opt-out (loop prevention), op subscription and
    /// the webhook's `projects` filter. Every dispatch path asks this one
    /// question, so a filtered project never reaches the endpoint on any path.
    fn subscribed(&self, hook: &ChainHook, ctx: &AdmissionContext) -> bool {
        if ctx.skip_webhooks.iter().any(|n| n == &hook.name) {
            tracing::debug!(webhook = %hook.name, "admission skip (caller opt-out)");
            return false;
        }
        hook.events.contains(&ctx.op) && hook.watches(ctx)
    }

    /// One notify-style POST (no body to send or mutate).
    async fn notify_once(
        &self,
        hook: &ChainHook,
        page_path: Option<&str>,
        ctx: &AdmissionContext,
    ) -> WikiResult<()> {
        let empty_frontmatter = serde_json::Value::Object(serde_json::Map::new());
        let payload = WebhookRequestBody {
            page: WebhookPagePayload {
                path: page_path.unwrap_or(""),
                frontmatter: &empty_frontmatter,
                body: "",
            },
            ctx,
        };
        let result = self
            .client
            .post(&hook.url)
            .headers(hook.headers.clone())
            .header("X-Memory-Op", ctx.op.as_header_value())
            .timeout(webhook_timeout(hook.timeout_ms))
            .json(&payload)
            .send()
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => {
                tracing::debug!(webhook = %hook.name, op = ctx.op.as_header_value(), "admission notify ok");
            }
            Ok(resp) => {
                let status = resp.status();
                let body_txt = response_text_limited(resp).await;
                tracing::warn!(webhook = %hook.name, status = %status, body = %body_txt, "admission notify error response");
                if matches!(hook.failure_policy, FailurePolicy::Reject) {
                    return Err(WikiError::Io(std::io::Error::other(format!(
                        "admission webhook {} status {}: {}",
                        hook.name, status, body_txt
                    ))));
                }
            }
            Err(e) => {
                let detail = format_request_error(&e);
                tracing::warn!(
                    webhook = %hook.name,
                    error = %detail,
                    "admission notify request failed",
                );
                if matches!(hook.failure_policy, FailurePolicy::Reject) {
                    return Err(WikiError::Io(std::io::Error::other(format!(
                        "admission webhook {} request failed: {detail}",
                        hook.name
                    ))));
                }
            }
        }
        Ok(())
    }

    /// Fire the chain's **non-blocking** webhooks for this op WITHOUT awaiting
    /// them. Called AFTER the page has landed on disk + in the index, so these
    /// are pure observers/mirrors: each is spawned fire-and-forget and its
    /// response is ignored (a non-blocking webhook can't mutate or reject).
    /// Honours the skip-list and op-subscription. For a write the caller passes
    /// the FINAL (post-mutation) page; for a delete/purge, an empty body.
    pub fn dispatch_async(
        &self,
        page_path: Option<&str>,
        frontmatter: &serde_json::Value,
        body: &str,
        ctx: &AdmissionContext,
    ) {
        for hook in self.webhooks.iter() {
            if hook.blocking {
                continue;
            }
            if !self.subscribed(hook, ctx) {
                continue;
            }
            self.spawn_webhook(hook, page_path, frontmatter, body, ctx);
        }
    }

    /// Spawn one webhook POST off the caller's path, bounded by the shared
    /// in-flight permit (hook paths must not fan out unboundedly).
    fn spawn_webhook(
        &self,
        hook: &ChainHook,
        page_path: Option<&str>,
        frontmatter: &serde_json::Value,
        body: &str,
        ctx: &AdmissionContext,
    ) {
        let Ok(permit) = self.async_semaphore.clone().try_acquire_owned() else {
            // A hook the operator marked `blocking` reaches this path too (see
            // MAX_ASYNC_ADMISSION_IN_FLIGHT), and dropping one contradicts what
            // that flag reads as, so it is not merely a warning: the operator
            // has to be able to find it in the log.
            if hook.blocking {
                tracing::error!(
                    webhook = %hook.name,
                    op = ctx.op.as_header_value(),
                    cap = MAX_ASYNC_ADMISSION_IN_FLIGHT,
                    "admission async queue saturated; dropping a webhook configured blocking",
                );
            } else {
                tracing::warn!(
                    webhook = %hook.name,
                    op = ctx.op.as_header_value(),
                    cap = MAX_ASYNC_ADMISSION_IN_FLIGHT,
                    "admission async queue saturated; dropping observer webhook",
                );
            }
            return;
        };
        let client = self.client.clone();
        let url = hook.url.clone();
        let headers = hook.headers.clone();
        let name = hook.name.clone();
        let timeout = hook.timeout_ms;
        let op = ctx.op;
        let owned_ctx = ctx.clone();
        let path = page_path.map(str::to_string);
        let frontmatter = frontmatter.clone();
        let body = body.to_string();
        tokio::spawn(async move {
            let _permit = permit;
            let payload = WebhookRequestBody {
                page: WebhookPagePayload {
                    path: path.as_deref().unwrap_or(""),
                    frontmatter: &frontmatter,
                    body: &body,
                },
                ctx: &owned_ctx,
            };
            match client
                .post(&url)
                .headers(headers)
                .header("X-Memory-Op", op.as_header_value())
                .timeout(webhook_timeout(timeout))
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    tracing::debug!(webhook = %name, op = op.as_header_value(), "admission async ok");
                }
                Ok(resp) => {
                    tracing::warn!(webhook = %name, status = %resp.status(), "admission async error response");
                }
                Err(e) => {
                    let detail = format_request_error(&e);
                    tracing::warn!(
                        webhook = %name,
                        error = %detail,
                        "admission async request failed",
                    );
                }
            }
        });
    }
}

/// `true` when this webhook's answer can change the outcome of an operation —
/// the operator asked for it to block AND to refuse on failure. Everything else
/// is an observer: under the default [`FailurePolicy::Ignore`] a non-2xx, a
/// timeout and an unreachable host all continue the operation unchanged.
fn decides(hook: &WebhookConfig) -> bool {
    hook.blocking && matches!(hook.failure_policy, FailurePolicy::Reject)
}

/// Accept only `http://` and `https://` admission webhook URLs.
///
/// Rejects `file://`, `ftp://`, custom schemes, and bare hostnames — any of
/// those would let a misconfigured or attacker-controlled operator config
/// send page payloads to the local filesystem, non-HTTP services, or
/// unresolvable targets. Relative URLs are also refused: every webhook must
/// be an absolute, network-addressable endpoint.
fn is_supported_webhook_url(url: &str) -> bool {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return false;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// Same redirect hop limit as `reqwest`'s default policy.
const MAX_WEBHOOK_REDIRECTS: usize = 10;

/// Follow a redirect only when it stays on the scheme, host, and port of the
/// previous request.
fn same_origin_redirects() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let same_origin = attempt
            .previous()
            .last()
            .is_some_and(|previous| previous.origin() == attempt.url().origin());
        if !same_origin {
            let target = attempt.url().origin().ascii_serialization();
            attempt.error(format!(
                "refused a redirect to another origin ({target}); set the endpoint to its final URL"
            ))
        } else if attempt.previous().len() > MAX_WEBHOOK_REDIRECTS {
            attempt.error("too many redirects")
        } else {
            attempt.follow()
        }
    })
}

/// Format a `reqwest::Error` together with its `Error::source` chain so
/// custom redirect-policy refusals and transport causes survive stringification.
fn format_request_error(err: &reqwest::Error) -> String {
    let mut message = err.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

fn webhook_timeout(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms.clamp(1, MAX_WEBHOOK_TIMEOUT_MS))
}

async fn read_response_limited(resp: reqwest::Response) -> Result<Option<Vec<u8>>, reqwest::Error> {
    let mut stream = resp.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Ok(None);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Some(bytes))
}

async fn response_text_limited(resp: reqwest::Response) -> String {
    match read_response_limited(resp).await {
        Ok(Some(bytes)) => String::from_utf8_lossy(&bytes).into_owned(),
        Ok(None) => format!("<response body exceeds {MAX_RESPONSE_BYTES} bytes>"),
        Err(e) => format!("<failed to read response body: {e}>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let pol = FailurePolicy::default();
        assert!(matches!(pol, FailurePolicy::Ignore));
        let op = AdmissionOp::default();
        assert!(matches!(op, AdmissionOp::WritePage));
    }

    #[test]
    fn purge_workspace_header_value_is_stable() {
        assert_eq!(
            AdmissionOp::PurgeWorkspace.as_header_value(),
            "purge_workspace"
        );
    }

    #[test]
    fn purge_workspace_event_deserializes_from_config_value() {
        let cfg: WebhookConfig = serde_json::from_value(serde_json::json!({
            "name": "mirror",
            "url": "http://127.0.0.1:1/hook",
            "events": ["purge_workspace"]
        }))
        .unwrap();
        assert_eq!(cfg.events, vec![AdmissionOp::PurgeWorkspace]);
    }

    #[test]
    fn webhook_timeout_is_clamped_to_smart_bounds() {
        assert_eq!(webhook_timeout(0), Duration::from_millis(1));
        assert_eq!(webhook_timeout(2_000), Duration::from_millis(2_000));
        assert_eq!(
            webhook_timeout(MAX_WEBHOOK_TIMEOUT_MS + 1),
            Duration::from_millis(MAX_WEBHOOK_TIMEOUT_MS)
        );
    }

    /// `partial_failure` serialises only when `true`, so existing webhook
    /// consumers see no extra key on the happy path. Mirrors that care
    /// about filesystem-vs-DB drift (a git-push mirror) opt in by reading
    /// the field when it appears.
    #[test]
    fn partial_failure_is_omitted_when_false() {
        let ctx = AdmissionContext {
            workspace: "w".into(),
            project: "p".into(),
            op: AdmissionOp::PurgeProject,
            ..Default::default()
        };
        let payload = serde_json::to_value(&ctx).unwrap();
        assert!(
            payload.get("partial_failure").is_none(),
            "partial_failure must be skipped when false: {payload}"
        );
    }

    #[test]
    fn partial_failure_serialises_when_true() {
        let ctx = AdmissionContext {
            workspace: "w".into(),
            project: "p".into(),
            op: AdmissionOp::PurgeProject,
            partial_failure: true,
            ..Default::default()
        };
        let payload = serde_json::to_value(&ctx).unwrap();
        assert_eq!(
            payload.get("partial_failure").and_then(|v| v.as_bool()),
            Some(true),
            "partial_failure must appear on the wire when true: {payload}"
        );
    }

    #[test]
    fn webhook_config_deserialises_with_defaults() {
        // Using JSON keeps the test free of an extra TOML dep — the
        // serde derives are format-agnostic so the same `#[serde(default)]`
        // handling exercised here covers TOML/YAML/etc.
        let json_src = serde_json::json!({
            "name": "contributors",
            "url": "http://contributors-webhook/enrich",
            "events": ["write_page", "consolidate"],
        });
        let cfg: WebhookConfig = serde_json::from_value(json_src).expect("parses");
        assert_eq!(cfg.name, "contributors");
        assert_eq!(cfg.timeout_ms, 2_000);
        assert!(matches!(cfg.failure_policy, FailurePolicy::Ignore));
        assert_eq!(cfg.events.len(), 2);
    }

    #[test]
    fn op_header_values() {
        assert_eq!(AdmissionOp::WritePage.as_header_value(), "write_page");
        assert_eq!(AdmissionOp::Consolidate.as_header_value(), "consolidate");
        assert_eq!(AdmissionOp::MoveProject.as_header_value(), "move_project");
        assert_eq!(AdmissionOp::MoveSession.as_header_value(), "move_session");
    }

    #[tokio::test]
    async fn empty_chain_is_noop() {
        let chain = AdmissionChain::new(vec![]).expect("builds");
        assert!(chain.is_empty());
        let mut md = Markdown {
            frontmatter: serde_json::Value::Null,
            body: "hello".to_string(),
        };
        let path = PagePath::new("foo.md").expect("valid path");
        let ctx = AdmissionContext::default();
        chain.run(&path, &mut md, &ctx).await.expect("noop");
        assert_eq!(md.body, "hello");
        assert!(md.frontmatter.is_null());
    }
}
