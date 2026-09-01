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

use std::sync::Arc;
use std::time::Duration;

use ai_memory_core::PagePath;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::error::WikiError;
use crate::{Markdown, WikiResult};

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
}

impl AdmissionOp {
    /// String value for the `X-Memory-Op` request header sent to webhooks.
    #[must_use]
    pub fn as_header_value(&self) -> &'static str {
        match self {
            AdmissionOp::WritePage => "write_page",
            AdmissionOp::Consolidate => "consolidate",
            AdmissionOp::Delete => "delete",
            AdmissionOp::PurgeProject => "purge_project",
            AdmissionOp::PurgeSession => "purge_session",
            AdmissionOp::PurgeWorkspace => "purge_workspace",
            AdmissionOp::MoveProject => "move_project",
            AdmissionOp::MoveSession => "move_session",
            AdmissionOp::HandoffBegin => "handoff_begin",
            AdmissionOp::HandoffAccept => "handoff_accept",
            AdmissionOp::HandoffCancel => "handoff_cancel",
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
    webhooks: Arc<Vec<WebhookConfig>>,
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
        let client = reqwest::Client::builder().build().map_err(|e| {
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
            // Caller-driven skip list (loop prevention via X-Memory-Skip-Admission-Chain).
            if ctx.skip_webhooks.iter().any(|n| n == &hook.name) {
                tracing::debug!(webhook = %hook.name, "admission skip (caller opt-out)");
                continue;
            }
            // Webhook doesn't subscribe to this op.
            if !hook.events.contains(&ctx.op) {
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
                        return Err(WikiError::Io(std::io::Error::other(err_msg)));
                    }
                }
                Err(e) => {
                    tracing::warn!(webhook = %hook.name, error = %e, "admission request failed");
                    if matches!(hook.failure_policy, FailurePolicy::Reject) {
                        return Err(WikiError::Io(std::io::Error::other(format!(
                            "admission webhook {} request failed: {}",
                            hook.name, e
                        ))));
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

    /// Shared filter: caller opt-out (loop prevention) plus op subscription.
    fn subscribed(&self, hook: &WebhookConfig, ctx: &AdmissionContext) -> bool {
        if ctx.skip_webhooks.iter().any(|n| n == &hook.name) {
            tracing::debug!(webhook = %hook.name, "admission skip (caller opt-out)");
            return false;
        }
        hook.events.contains(&ctx.op)
    }

    /// One notify-style POST (no body to send or mutate).
    async fn notify_once(
        &self,
        hook: &WebhookConfig,
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
                tracing::warn!(webhook = %hook.name, error = %e, "admission notify request failed");
                if matches!(hook.failure_policy, FailurePolicy::Reject) {
                    return Err(WikiError::Io(std::io::Error::other(format!(
                        "admission webhook {} request failed: {}",
                        hook.name, e
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
            if ctx.skip_webhooks.iter().any(|n| n == &hook.name) {
                continue;
            }
            if !hook.events.contains(&ctx.op) {
                continue;
            }
            self.spawn_webhook(hook, page_path, frontmatter, body, ctx);
        }
    }

    /// Spawn one webhook POST off the caller's path, bounded by the shared
    /// in-flight permit (hook paths must not fan out unboundedly).
    fn spawn_webhook(
        &self,
        hook: &WebhookConfig,
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
                    tracing::warn!(webhook = %name, error = %e, "admission async request failed");
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
