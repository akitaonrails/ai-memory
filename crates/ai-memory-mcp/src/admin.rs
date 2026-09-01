//! Admin HTTP routes — state-touching operations invoked by the CLI
//! over plain HTTP (not MCP). Currently exposes:
//!
//! - `POST /admin/backup`         — snapshot db + wiki into a gzip tarball (binary response).
//! - `POST /admin/bootstrap`      — ingest a pre-collected source bundle
//!   into seed wiki pages via the configured LLM provider.
//! - `POST /admin/auto-improve`   — review one session and apply or stage proposals.
//! - `POST /admin/auto-improve/report` — read-only auto-improve telemetry report.
//! - `POST /admin/curator`        — dry-run or stage a rule-based curator report.
//! - `GET  /admin/status`         — lifetime counts + server data-dir info.
//! - `GET  /admin/projects`       — authoritative `(workspace, project)` list.
//! - `GET  /admin/open-sessions`  — open (not yet ended) sessions for one scope + agent.
//! - `GET  /admin/sessions/by-agent` — session counts per agent CLI for one scope.
//! - `GET  /admin/activity/by-client` — MCP tool-call counts per client (server-wide).
//! - `GET  /admin/audit-log`      — paginated read of the append-only `audit_log`.
//! - `GET  /admin/search?q=`      — FTS5 hits against the wiki index.
//! - `POST /admin/reorg`          — retro-fit sessions to per-cwd projects.
//! - `POST /admin/lint`           — run the M8 lint pass.
//! - `POST /admin/forget-sweep`   — run the M8 retention sweep.
//! - `POST /admin/embed`          — backfill embeddings for latest pages.
//! - `POST /admin/commit`         — stage + commit the wiki tree via git.
//! - `GET  /admin/checkpoints`    — list recent wiki git checkpoints.
//! - `POST /admin/restore-page`   — restore one page from a checkpoint.
//! - `POST /admin/purge-project`  — delete a project's rows and wiki files
//!   (logical unless `compact` is set; see `ai_memory_store::Compaction`).
//! - `POST /admin/purge-session`  — delete one session and what it derived.
//! - `POST /admin/rename-project` — rename a project (column-only; no files move).
//! - `POST /admin/rename-workspace` — rename a workspace and refresh scope manifests.
//! - `POST /admin/compact`        — reclaim free pages; deletes nothing.
//! - `POST /admin/delete-workspace` — delete a workspace and its projects
//!   (logical unless `compact` is set; see `ai_memory_store::Compaction`).
//! - `POST /admin/merge-workspace` — fold every project of one workspace into
//!   another, then delete the emptied source workspace.
//! - `POST /admin/move-project`   — move a project into another workspace
//!   (copy latest pages via the write path, then purge the source).
//! - `POST /admin/write-page`     — write or update a wiki page atomically.
//! - `GET  /admin/read-page`      — fetch the full body of a single wiki page by path.
//! - `POST /admin/delete-page`    — delete a single wiki page by path.
//!
//! The CLI is responsible for filesystem access (collecting sources from
//! the project repo, rendering output for humans); the server is
//! responsible for all state reads/writes against the wiki + SQLite.

use std::sync::Arc;

use std::future::Future;
use std::io::Seek;
use std::path::PathBuf;
use std::pin::Pin;

use ai_memory_consolidate::{
    AutoImproveReviewConfig, AutoImproveTelemetryParams, AutoImproveTelemetryReport, Bootstrap,
    BootstrapConfig, BootstrapOutcome, BootstrapSource, CuratorParams, CuratorReport,
    EmbedBackfillCounts, EmbedBackfillOptions, ObservationRetention, SourceCounts,
    prune_sources_to_budget, render_auto_improve_telemetry_report_markdown,
    render_curator_report_markdown, run_auto_improve_review, run_auto_improve_telemetry_report,
    run_curator_report_with_breadth, run_embedding_backfill, run_lint, run_sweep_with_options,
};
use ai_memory_core::{
    ActiveProject, AgentKind, AutoImproveProposalId, Capability, DEFAULT_PROJECT_NAME,
    DEFAULT_WORKSPACE_NAME, PagePath, ProjectId, SessionId, Tier, WorkspaceId,
};
use ai_memory_llm::{Embedder, LlmProvider, ProviderHealth, ProviderHealthSnapshot};
use ai_memory_store::{
    ApproveAutoImproveProposalResult, AuditLogFilter, AutoImproveProposalOperation,
    AutoImproveProposalStatus, DecayParams, NewAutoImproveProposal, PagesMode, ReaderPool,
    RejectAutoImproveProposal, ScopeResolutionError, SkippedProposal, StageAutoImproveRun,
    StoreError, WriterHandle, create_explicit_scope, f32_vec_to_bytes, lookup_existing_scope,
    lookup_existing_workspace,
};
use ai_memory_wiki::{
    AdmissionContext, AdmissionOp, Markdown, SessionPageFile, Wiki, WikiError, WritePageRequest,
};
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;
use tracing::{info, warn};

const CONTRIBUTORS_WEBHOOK_NAME: &str = "contributors";

/// Sweep knobs that live beside `DecayParams` rather than inside it, injected
/// as one Extension so adding the next one is not a third router constructor.
#[derive(Clone, Copy, Default)]
struct SweepTuning {
    breadth_weight: f64,
    retention: ObservationRetention,
}

/// Shared state for the admin router.
#[derive(Clone)]
pub struct AdminState {
    /// Writer actor handle — used to get-or-create workspace/project.
    pub writer: WriterHandle,
    /// Reader pool — used by the idempotency check inside Bootstrap.
    pub reader: ReaderPool,
    /// Wiki handle — pages are written here.
    pub wiki: Wiki,
    /// Optional LLM provider. When `None`, bootstrap returns 503.
    pub llm: Option<Arc<dyn LlmProvider>>,
    /// If true, auto-improve leaves staged proposals pending for manual
    /// approval; otherwise it approves them immediately through the normal wiki
    /// write path.
    pub auto_improve_require_approval: bool,
    /// Server-configured defaults used by direct admin auto-improvement.
    /// Request fields remain explicit overrides; omitted eval settings inherit
    /// this operator policy instead of disabling eval silently.
    pub auto_improve_review_config: AutoImproveReviewConfig,
    /// Optional embedder. When `None`, `/admin/embed` returns 503.
    pub embedder: Option<Arc<dyn Embedder>>,
    /// Passive process-scoped health recorder for configured providers.
    pub provider_health: ProviderHealth,
    /// Shared hook-ingestion counters, written by the hook path and read
    /// here for status reporting.
    pub ingest_metrics: std::sync::Arc<ai_memory_core::IngestMetrics>,
    /// Retention-decay parameters forwarded from server config.
    pub decay_params: DecayParams,
    /// Server's resolved data directory (e.g. `/data` in the docker
    /// image). Surfaced via `/admin/status` so the CLI can report
    /// "where the wiki + db actually live".
    pub data_dir: PathBuf,
    /// Resolved SQLite path inside `data_dir`. Same purpose as above.
    pub db_path: PathBuf,
    /// Server's bind address — informational, surfaced in /admin/status.
    pub bind: String,
    /// Server home directory resolved once at config load. Used to keep admin
    /// audits consistent with the hook router's cwd-prefix guard.
    pub home_dir: Option<String>,
    /// Serialises concurrent bootstrap requests. Bootstrap fans out
    /// into an LLM call + a multi-page wiki write + a git commit;
    /// running two in parallel would race the `commit_all` git ops and
    /// stack LLM cost unnecessarily. The mutex is held for the entire
    /// handler so a second caller waits its turn (request stays open
    /// until the lock is acquired, then proceeds normally).
    pub bootstrap_lock: Arc<tokio::sync::Mutex<()>>,
    /// Per-server token pepper, used by `/admin/api-credentials` to hash
    /// freshly-issued `aim_` secrets before they land in
    /// `api_credentials.token_hash`. `None` when the operator hasn't set
    /// `[auth].token_pepper` in config (single-user installs that predate
    /// v0.8); native API-credential endpoints then return 503
    /// `multi-user not enabled`.
    pub token_pepper: Option<ai_memory_store::TokenPepper>,
    /// Shared in-process pointer to the project the agent is currently
    /// active in (published by the hook router). Read by `move-project` to
    /// refuse moving the live project (unless `force`) and to keep the
    /// pointer correct after a move. Empty `ActiveProject::new()` when no
    /// hook router is attached (admin-only tests).
    pub active_project: ActiveProject,
    /// Optional hook to PROACTIVELY evict the hook router's per-cwd
    /// `(workspace_id, project_id)` cache after admin scope mutations. Admin
    /// handlers await this after successful moves/purges/deletes/renames so
    /// the next hook event re-resolves cleanly instead of racing through a
    /// stale cached pair. `None` when no hook router is attached (stdio /
    /// admin-only tests).
    pub scope_invalidator: Option<ScopeInvalidator>,
    /// True when a trusted authenticating proxy is configured to assert
    /// end-user identities (`[auth].actor_proxy_bearer_token`).
    ///
    /// Proxy-asserted operators never get a `users` row, so `users_exist()`
    /// alone answers "does this deployment distinguish operators" with a
    /// permanent no — and every proxied caller walks through the
    /// single-operator escape hatch on the `/admin/*` gate. Static config, set
    /// once at startup; `false` for every deployment that never configures a
    /// proxy secret, which is what keeps single-operator servers on their
    /// historical behaviour.
    pub trusted_proxy_identity: bool,
}

/// Async hook-router project-cache invalidator installed by the serve command.
pub type ScopeInvalidator = Arc<
    dyn Fn(ScopeInvalidation) -> Pin<Box<dyn Future<Output = ()> + Send + 'static>> + Send + Sync,
>;

/// Hook-router project-cache invalidation target for admin scope mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeInvalidation {
    /// Evict cache entries whose resolved project id matches.
    Project(ProjectId),
    /// Evict cache entries whose resolved workspace id matches.
    Workspace(WorkspaceId),
}

async fn invalidate_scope_cache(state: &AdminState, target: ScopeInvalidation) {
    if let Some(evict) = &state.scope_invalidator {
        evict(target).await;
    }
}

/// JSON request body for `POST /admin/bootstrap`.
#[derive(Deserialize)]
struct BootstrapRequest {
    /// Workspace name (auto-created if it doesn't exist).
    workspace: String,
    /// Project name (auto-created if it doesn't exist).
    project: String,
    /// Sources pre-collected on the client side.
    sources: Vec<BootstrapSource>,
    /// Original collection size before client-side prune (if any).
    #[serde(default)]
    sources_collected: Option<usize>,
    /// Maximum input tokens for LLM call.
    #[serde(default = "default_max_input_tokens")]
    max_input_tokens: usize,
    /// Per-LLM-call input cap; larger bundles are split into chunks.
    #[serde(default = "default_chunk_input_tokens")]
    chunk_input_tokens: usize,
    /// Skip the LLM call and page writes — returns a dry-run outcome.
    #[serde(default)]
    dry_run: bool,
    /// Allow re-bootstrap when `wiki/bootstrap.md` already exists.
    #[serde(default)]
    force: bool,
}

fn default_max_input_tokens() -> usize {
    50_000
}

fn default_chunk_input_tokens() -> usize {
    ai_memory_consolidate::DEFAULT_CHUNK_INPUT_TOKENS
}

/// JSON request body for `POST /admin/auto-improve`.
#[derive(Deserialize)]
struct AutoImproveRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Completed session to review.
    session_id: SessionId,
    /// Minimum observations before the LLM review runs.
    #[serde(default = "default_auto_improve_min_observations")]
    min_observations: usize,
    /// Minimum session span before the LLM review runs.
    #[serde(default = "default_auto_improve_min_session_duration_secs")]
    min_session_duration_secs: u64,
    /// Minimum proposal confidence accepted by validation.
    #[serde(default = "default_auto_improve_min_confidence")]
    min_confidence: f32,
    /// Approximate chars/4 prompt budget.
    #[serde(default = "default_auto_improve_max_input_tokens")]
    max_input_tokens: usize,
    /// Maximum validated proposals returned.
    #[serde(default = "default_auto_improve_max_proposals")]
    max_proposals_per_run: usize,
    /// Whether future raw fallback details may be considered.
    #[serde(default)]
    include_raw_fallback: bool,
    /// Maximum existing _rules/ and procedures/ pages included for patch proposals.
    #[serde(default = "default_auto_improve_max_patchable_pages")]
    max_patchable_pages: usize,
    /// Maximum body chars rendered per patchable target page.
    #[serde(default = "default_auto_improve_max_patchable_body_chars")]
    max_patchable_body_chars: usize,
    /// Maximum patch edits per proposal.
    #[serde(default = "default_auto_improve_max_edits_per_proposal")]
    max_edits_per_proposal: usize,
    /// Maximum content chars in one patch edit.
    #[serde(default = "default_auto_improve_max_edit_content_chars")]
    max_edit_content_chars: usize,
    /// Maximum aggregate changed chars in one patch proposal.
    #[serde(default = "default_auto_improve_max_changed_chars_per_proposal")]
    max_changed_chars_per_proposal: usize,
    /// Maximum patch edits accepted across one review run.
    #[serde(default = "default_auto_improve_max_patch_edits_per_run")]
    max_patch_edits_per_run: usize,
    /// Maximum recent rejection-buffer entries rendered into prompt context.
    #[serde(default = "default_auto_improve_max_rejection_context")]
    max_rejection_context: usize,
    /// Maximum age in days for rejection-buffer prompt context.
    #[serde(default = "default_auto_improve_rejection_context_days")]
    rejection_context_days: u32,
    /// Maximum materialized final body size.
    #[serde(default = "default_auto_improve_max_final_body_chars")]
    max_final_body_chars: usize,
    /// Maximum approximate tokens allowed in one _rules/ page.
    #[serde(default = "default_auto_improve_max_rule_page_tokens")]
    max_rule_page_tokens: usize,
    /// Maximum approximate tokens allowed in one procedures/ page.
    #[serde(default = "default_auto_improve_max_procedure_page_tokens")]
    max_procedure_page_tokens: usize,
    /// Synthetic actor used for staged proposal provenance.
    #[serde(default = "default_auto_improve_proposal_actor")]
    proposal_actor: String,
    /// Pending proposal sidecar folder.
    #[serde(default = "default_auto_improve_pending_path")]
    pending_path: String,
    /// Optional executable proposal eval gate. When omitted, direct admin
    /// requests inherit the server's configured eval policy; CLI requests send
    /// their local config explicitly.
    #[serde(default)]
    eval: Option<ai_memory_consolidate::AutoImproveEvalConfig>,
    /// Removed compatibility field. Requests that still send it fail closed so
    /// old preview callers cannot accidentally write pages.
    #[serde(default)]
    dry_run: Option<bool>,
    /// Removed compatibility field.
    #[serde(default)]
    stage: Option<bool>,
    /// Removed compatibility field.
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct AutoImproveStageResponse {
    run_id: String,
    approval_required: bool,
    approval_policy: String,
    session_id: String,
    summary: String,
    warnings: Vec<String>,
    rejected_candidates_count: usize,
    proposals: Vec<AutoImproveProposalOutcome>,
    /// Proposals the reviewer produced but the store did not stage, with the
    /// reason. Always present (empty on a clean run) so a consumer can tell
    /// "nothing was dropped" from "this build does not report drops".
    skipped: Vec<SkippedProposal>,
}

#[derive(Debug, Serialize)]
struct AutoImproveProposalOutcome {
    id: String,
    sidecar_path: String,
    status: String,
    page_id: Option<String>,
}

/// JSON request body for `POST /admin/auto-improve/report`.
#[derive(Deserialize)]
struct AutoImproveTelemetryReportRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Lookback window in days.
    #[serde(default = "default_auto_improve_telemetry_since_days")]
    since_days: u32,
    /// Maximum rows in each top-N count table.
    #[serde(default = "default_auto_improve_telemetry_top_limit")]
    limit: usize,
    /// Stage one pending telemetry report page instead of returning a read-only report.
    #[serde(default)]
    stage: bool,
}

#[derive(Debug, Serialize)]
struct AutoImproveTelemetryReportStageResponse {
    run_id: String,
    proposal_ids: Vec<String>,
    sidecar_paths: Vec<String>,
    /// Same reason as [`AutoImproveStageResponse::skipped`]: a single-proposal
    /// run that collides otherwise returns an empty `proposal_ids` and no clue.
    skipped: Vec<SkippedProposal>,
    report: AutoImproveTelemetryReport,
}

/// JSON request body for `POST /admin/curator`.
#[derive(Deserialize)]
struct CuratorRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Return a report without staging anything.
    #[serde(default)]
    dry_run: bool,
    /// Stage one pending report page.
    #[serde(default)]
    stage: bool,
    /// Optional mode alias (`dry_run` or `stage`).
    #[serde(default)]
    mode: Option<String>,
}

#[derive(Debug, Serialize)]
struct CuratorStageResponse {
    run_id: String,
    proposal_ids: Vec<String>,
    sidecar_paths: Vec<String>,
    /// Same reason as [`AutoImproveStageResponse::skipped`]: a single-proposal
    /// run that collides otherwise returns an empty `proposal_ids` and no clue.
    skipped: Vec<SkippedProposal>,
    report: CuratorReport,
}

/// Query for `GET /admin/handoffs` (#513).
#[derive(Debug, Deserialize)]
struct OpenHandoffsQuery {
    workspace: String,
    project: String,
    #[serde(default = "default_open_handoffs_limit")]
    limit: usize,
}

const fn default_open_handoffs_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
struct PendingWritesQuery {
    workspace: String,
    project: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default = "default_pending_writes_limit")]
    limit: usize,
}

fn default_pending_writes_limit() -> usize {
    50
}

#[derive(Debug, Deserialize)]
struct PendingWriteScopeQuery {
    workspace: String,
    project: String,
}

#[derive(Debug, Deserialize)]
struct PendingRejectRequest {
    #[serde(default)]
    reason: String,
}

fn default_auto_improve_min_observations() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MIN_OBSERVATIONS
}

fn default_auto_improve_min_session_duration_secs() -> u64 {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MIN_SESSION_DURATION_SECS
}

fn default_auto_improve_min_confidence() -> f32 {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE
}

fn default_auto_improve_max_input_tokens() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_INPUT_TOKENS
}

fn default_auto_improve_max_proposals() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PROPOSALS
}

fn default_auto_improve_max_patchable_pages() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_PAGES
}

fn default_auto_improve_max_patchable_body_chars() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_BODY_CHARS
}

fn default_auto_improve_max_edits_per_proposal() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_EDITS_PER_PROPOSAL
}

fn default_auto_improve_max_edit_content_chars() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_EDIT_CONTENT_CHARS
}

fn default_auto_improve_max_changed_chars_per_proposal() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_CHANGED_CHARS_PER_PROPOSAL
}

fn default_auto_improve_max_patch_edits_per_run() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PATCH_EDITS_PER_RUN
}

fn default_auto_improve_max_rejection_context() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_REJECTION_CONTEXT
}

fn default_auto_improve_rejection_context_days() -> u32 {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_REJECTION_CONTEXT_DAYS
}

fn default_auto_improve_max_final_body_chars() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_FINAL_BODY_CHARS
}

fn default_auto_improve_max_rule_page_tokens() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_RULE_PAGE_TOKENS
}

fn default_auto_improve_max_procedure_page_tokens() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_MAX_PROCEDURE_PAGE_TOKENS
}

fn default_auto_improve_proposal_actor() -> String {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_PROPOSAL_ACTOR.into()
}

fn default_auto_improve_pending_path() -> String {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_PENDING_PATH.into()
}

fn default_auto_improve_telemetry_since_days() -> u32 {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_TELEMETRY_SINCE_DAYS
}

fn default_auto_improve_telemetry_top_limit() -> usize {
    ai_memory_consolidate::DEFAULT_AUTO_IMPROVE_TELEMETRY_TOP_LIMIT
}

fn hex_to_sha256(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err("expected 64 hex chars".into());
    }
    let mut out = [0_u8; 32];
    for (idx, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|e| e.to_string())?;
        out[idx] = u8::from_str_radix(s, 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Build the admin axum [`Router`]. Mounts:
/// - `POST /admin/backup`
/// - `POST /admin/bootstrap`
/// - `POST /admin/auto-improve`
/// - `POST /admin/auto-improve/report`
/// - `POST /admin/curator`
/// - `GET  /admin/status`
/// - `GET  /admin/projects`
/// - `GET  /admin/open-sessions`
/// - `GET  /admin/sessions/by-agent`
/// - `GET  /admin/activity/by-client`
/// - `GET  /admin/audit-contamination`
/// - `GET  /admin/search`
/// - `GET  /admin/read-page`
/// - `POST /admin/reorg`
/// - `POST /admin/lint`
/// - `POST /admin/forget-sweep`
/// - `POST /admin/embed`
/// - `POST /admin/commit`
/// - `GET  /admin/checkpoints`
/// - `POST /admin/restore-page`
/// - `POST /admin/purge-project`
/// - `POST /admin/rename-project`
/// - `POST /admin/move-project`
/// - `POST /admin/move-session`
/// - `POST /admin/merge-workspace`
/// - `POST /admin/write-page`
/// - `POST /admin/delete-page`
/// - user-management routes under `/admin/users*`
pub fn admin_router(state: AdminState) -> Router {
    admin_router_with_decay_breadth(state, 0.0)
}

/// Build the admin router with the optional distinct-reader retention weight.
pub fn admin_router_with_decay_breadth(state: AdminState, breadth_weight: f64) -> Router {
    admin_router_with_sweep_tuning(state, breadth_weight, ObservationRetention::default())
}

/// Build the admin router with every sweep knob that lives outside
/// `DecayParams`, including the opt-in observation prune (disabled by default).
pub fn admin_router_with_sweep_tuning(
    state: AdminState,
    breadth_weight: f64,
    retention: ObservationRetention,
) -> Router {
    let state = Arc::new(state);
    let operational = Router::new()
        .route("/admin/backup", post(handle_backup))
        .route("/admin/bootstrap", post(handle_bootstrap))
        .route("/admin/auto-improve", post(handle_auto_improve))
        .route(
            "/admin/auto-improve/report",
            post(handle_auto_improve_report),
        )
        .route("/admin/curator", post(handle_curator))
        .route("/admin/handoffs", get(handle_open_handoffs_list))
        .route("/admin/handoffs/expire", post(handle_expire_handoffs))
        .route("/admin/pending-writes", get(handle_pending_writes_list))
        .route(
            "/admin/pending-writes/{id}",
            get(handle_pending_write_detail),
        )
        .route(
            "/admin/pending-writes/{id}/diff",
            get(handle_pending_write_diff),
        )
        .route(
            "/admin/pending-writes/{id}/approve",
            post(handle_pending_write_approve),
        )
        .route(
            "/admin/pending-writes/{id}/reject",
            post(handle_pending_write_reject),
        )
        .route("/admin/status", get(handle_status))
        .route("/admin/projects", get(handle_list_projects))
        .route("/admin/open-sessions", get(handle_open_sessions))
        .route("/admin/sessions/by-agent", get(handle_sessions_by_agent))
        .route("/admin/activity/by-client", get(handle_activity_by_client))
        .route(
            "/admin/audit-contamination",
            get(handle_audit_contamination),
        )
        .route("/admin/audit-log", get(handle_audit_log))
        .route("/admin/search", get(handle_search))
        .route("/admin/read-page", get(handle_read_page))
        .route("/admin/reorg", post(handle_reorg))
        .route("/admin/lint", post(handle_lint))
        .route("/admin/forget-sweep", post(handle_forget_sweep))
        .route("/admin/embed", post(handle_embed))
        .route("/admin/commit", post(handle_commit))
        .route("/admin/checkpoints", get(handle_checkpoints))
        .route("/admin/restore-page", post(handle_restore_page))
        .route("/admin/purge-project", post(handle_purge_project))
        .route("/admin/purge-session", post(handle_purge_session))
        .route("/admin/rename-project", post(handle_rename_project))
        .route("/admin/move-project", post(handle_move_project))
        .route("/admin/move-session", post(handle_move_session))
        .route("/admin/delete-workspace", post(handle_delete_workspace))
        .route("/admin/compact", post(handle_compact))
        .route("/admin/rename-workspace", post(handle_rename_workspace))
        .route("/admin/merge-workspace", post(handle_merge_workspace))
        .route("/admin/write-page", post(handle_write_page))
        .route("/admin/delete-page", post(handle_delete_page));
    let users = Router::new()
        .route(
            "/admin/users",
            get(handle_list_users).post(handle_create_user),
        )
        .route("/admin/human-users", post(handle_create_human_user))
        .route("/admin/users/{username}", patch(handle_patch_user))
        .route("/admin/users/{username}/expire", post(handle_expire_user))
        .route("/admin/users/{username}/revive", post(handle_revive_user))
        .route(
            "/admin/users/{username}/rotate-token",
            post(handle_rotate_user_token),
        )
        .route(
            "/admin/users/{username}/reset-password",
            post(handle_reset_password),
        )
        .route("/admin/users/{username}/disable", post(handle_disable_user))
        .route("/admin/users/{username}/enable", post(handle_enable_user))
        .route(
            "/admin/api-credentials",
            get(handle_list_api_credentials).post(handle_create_api_credential),
        )
        .route(
            "/admin/api-credentials/{id}/rotate",
            post(handle_rotate_api_credential),
        )
        .route(
            "/admin/api-credentials/{id}/revoke",
            post(handle_revoke_api_credential),
        );
    operational
        .merge(users)
        .route_layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_root_for_multiuser_admin,
        ))
        .with_state(state)
        .layer(axum::Extension(SweepTuning {
            breadth_weight,
            retention,
        }))
}

async fn require_root_for_multiuser_admin(
    State(state): State<Arc<AdminState>>,
    req: axum::http::Request<Body>,
    next: Next,
) -> Response {
    let level = req
        .extensions()
        .get::<ai_memory_core::AuthLevel>()
        .copied()
        .unwrap_or(ai_memory_core::AuthLevel::Anonymous);
    // Read this for every request rather than caching a post-creation flag: a
    // committed first user immediately closes bootstrap admin access, including
    // across concurrent requests and without a server restart.
    //
    // Same notion of "this deployment distinguishes operators" the MCP admin
    // gate uses. Keyed on `users_exist()` alone, a trusted-proxy deployment
    // with no `users` rows would wave a proxy-asserted `AuthLevel::User` caller
    // through every operational route below — purge, delete-page, backup — and
    // only `/admin/users*` would survive on its own inner root check.
    let distinguishes_operators = match state
        .reader
        .distinguishes_operators(state.trusted_proxy_identity)
        .await
    {
        Ok(exists) => exists,
        Err(error) => {
            tracing::error!(%error, "admin authorization could not determine whether users exist");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "admin authorization unavailable: could not verify user configuration"
                })),
            )
                .into_response();
        }
    };
    match level.authorize(Capability::Admin, distinguishes_operators) {
        Ok(()) => next.run(req).await,
        Err(e) => (
            authz_status(e),
            Json(serde_json::json!({ "error": e.message() })),
        )
            .into_response(),
    }
}

fn authz_status(err: ai_memory_core::AuthzError) -> StatusCode {
    if err.is_authentication_required() {
        StatusCode::UNAUTHORIZED
    } else {
        StatusCode::FORBIDDEN
    }
}

// ---------------------------------------------------------------------
// backup
// ---------------------------------------------------------------------

/// Handler for `POST /admin/backup`.
///
/// Snapshots the live SQLite DB via the online backup API, then
/// tar-gzips `wiki/`, the snapshot, and `config.toml` (if present)
/// into a tempfile. The response streams the file instead of buffering
/// the full archive in memory.
async fn handle_backup(State(state): State<Arc<AdminState>>) -> Response {
    match build_backup_tarball_file(&state).await {
        Ok(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/gzip")
            .header(
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"backup.tar.gz\"",
            )
            .body(Body::from_stream(ReaderStream::new(file)))
            .unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::empty())
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
            }),
        Err(e) => {
            warn!(error = %e, "backup failed");
            let body = serde_json::to_vec(&serde_json::json!({ "error": e.to_string() }))
                .unwrap_or_default();
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap_or_else(|_| {
                    Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(Body::empty())
                        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
                })
        }
    }
}

async fn build_backup_tarball_file(state: &AdminState) -> anyhow::Result<tokio::fs::File> {
    let staging = tempfile::tempdir()?;
    let snapshot_path = staging.path().join("memory.sqlite");
    info!(snapshot = %snapshot_path.display(), "snapshotting SQLite for backup");
    state
        .reader
        .snapshot_to(snapshot_path.clone())
        .await
        .map_err(|e| anyhow::anyhow!("sqlite snapshot: {e}"))?;

    let mut tar_file = tempfile::tempfile()?;
    {
        let encoder = GzEncoder::new(&mut tar_file, Compression::default());
        let mut tar = tar::Builder::new(encoder);
        tar.mode(tar::HeaderMode::Deterministic);
        tar.follow_symlinks(false);

        let wiki_dir = state.data_dir.join("wiki");
        if wiki_dir.is_dir() {
            tar.append_dir_all("wiki", &wiki_dir)
                .map_err(|e| anyhow::anyhow!("archiving wiki/: {e}"))?;
        }

        tar.append_path_with_name(&snapshot_path, "db/memory.sqlite")
            .map_err(|e| anyhow::anyhow!("archiving db snapshot: {e}"))?;

        let cfg = state.data_dir.join("config.toml");
        if cfg.is_file() {
            tar.append_path_with_name(&cfg, "config.toml")
                .map_err(|e| anyhow::anyhow!("archiving config.toml: {e}"))?;
        }

        let encoder = tar.into_inner()?;
        encoder.finish()?;
    }
    tar_file.sync_data()?;
    tar_file.rewind()?;
    Ok(tokio::fs::File::from_std(tar_file))
}

// ---------------------------------------------------------------------
// status
// ---------------------------------------------------------------------

/// Query string for `GET /admin/audit-contamination`. Supplying BOTH
/// `workspace` and `project` scopes the audit to that one landed bucket;
/// omit both to audit every project.
#[derive(Deserialize)]
struct AuditContaminationQuery {
    workspace: Option<String>,
    project: Option<String>,
}

/// `GET /admin/audit-contamination` — read-only structural contamination audit
/// (see [`ai_memory_store::ReaderPool::audit_contamination`]). Reports likely
/// cross-project mislandings (a session whose cwd resolves elsewhere; an
/// observation whose project disagrees with its session); never mutates, so it
/// is safe to run on any cadence (e.g. a cron probe alerting on non-zero counts).
async fn handle_audit_contamination(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<AuditContaminationQuery>,
) -> impl IntoResponse {
    let scope = match (
        trimmed_opt(q.workspace.as_deref()),
        trimmed_opt(q.project.as_deref()),
    ) {
        (Some(ws), Some(proj)) => match lookup_ws_proj_no_create(&state, ws, proj).await {
            Ok(ids) => Some(ids),
            Err(e) => return e,
        },
        (Some(_), None) | (None, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "workspace and project must be provided together"
                })),
            );
        }
        _ => None,
    };
    match state
        .reader
        .audit_contamination(scope, state.home_dir.as_deref())
        .await
    {
        Ok(report) => (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// Query string for `GET /admin/audit-log`. Names are optional independent
/// filters; when both workspace and project are present they are resolved
/// through [`lookup_existing_scope`] so a typo fails closed (404) instead of
/// looking like an empty log.
#[derive(Debug, Deserialize)]
struct AuditLogQuery {
    workspace: Option<String>,
    project: Option<String>,
    op: Option<String>,
    before_id: Option<i64>,
    #[serde(default = "default_audit_log_limit")]
    limit: usize,
}

fn default_audit_log_limit() -> usize {
    50
}

/// `GET /admin/audit-log` — read-only paginated trail of the existing
/// `audit_log` table. `detail` is returned for schema fidelity; it is not a
/// payload (the only writer stores the literal `{}`).
async fn handle_audit_log(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<AuditLogQuery>,
) -> impl IntoResponse {
    let workspace = trimmed_opt(q.workspace.as_deref()).map(str::to_owned);
    let project = trimmed_opt(q.project.as_deref()).map(str::to_owned);
    if let (Some(ws), Some(proj)) = (workspace.as_deref(), project.as_deref()) {
        match lookup_ws_proj_no_create(&state, ws, proj).await {
            Ok(_) => {}
            Err(e) => return e,
        }
    }
    let filter = AuditLogFilter {
        workspace,
        project,
        op: trimmed_opt(q.op.as_deref()).map(str::to_owned),
        before_id: q.before_id,
        limit: q.limit,
    };
    match state.reader.list_audit_events(filter).await {
        Ok(events) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(&events)
                    .map(|list| serde_json::json!({ "events": list }))
                    .unwrap_or_else(|_| serde_json::json!({ "events": [] })),
            ),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// JSON response body for `GET /admin/status`. The CLI's `status`
/// subcommand renders this either as JSON (`--json`) or as a small
/// human-friendly text block.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    /// Server binary version (Cargo package version).
    pub version: String,
    /// Absolute data directory the server uses (server-side path).
    pub data_dir: String,
    /// Bind address the HTTP transport is listening on.
    pub bind: String,
    /// Absolute SQLite path inside `data_dir`.
    pub db_path: String,
    /// Lifetime counts: pages_latest, pages_all, sessions, observations.
    pub counts: ai_memory_store::StatusCounts,
    /// Derived-index and retrieval-readiness diagnostics.
    pub derived: ai_memory_store::DerivedIndexStatus,
    /// Physical storage figures — file size and reclaimable free pages — so an
    /// operator can decide whether a `VACUUM` is worth its exclusive lock.
    pub storage: ai_memory_store::StorageStatus,
    /// Passive process-scoped provider health.
    pub providers: ProviderHealthSnapshot,
    /// Hook-ingestion counters for this server process. Counts and one
    /// timestamp only — never captured content (#428).
    pub ingest: ai_memory_core::IngestMetricsSnapshot,
}

/// `GET /admin/projects` — the authoritative list of `(workspace, project)`
/// pairs (with page counts + last-updated), read-only. Useful to any external
/// consumer that needs the live project inventory: a dashboard, an export, or
/// a backup/mirror that lays the wiki out as `wiki/<workspace>/<project>/` and
/// wants to reconcile against it — any directory whose project no longer
/// appears here is an orphan (e.g. from a delete that bypassed the admission
/// chain, such as an offline `--data-dir` purge or a direct DB edit) and can
/// be pruned so the copy reflects the live server.
async fn handle_list_projects(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    match state.reader.list_projects_with_stats().await {
        Ok(projects) => (
            StatusCode::OK,
            Json(serde_json::json!({ "projects": projects })),
        ),
        Err(e) => internal_err(e.to_string()),
    }
}

async fn handle_status(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    match state.reader.status_counts().await {
        Ok(counts) => match state.reader.derived_index_status().await {
            Ok(derived) => {
                // Three pragma reads; a failure here must not take down the
                // whole status response, which is also the health probe.
                let storage = state.reader.storage_status().await.unwrap_or_default();
                let report = StatusReport {
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    data_dir: state.data_dir.display().to_string(),
                    bind: state.bind.clone(),
                    db_path: state.db_path.display().to_string(),
                    counts,
                    derived,
                    storage,
                    providers: state.provider_health.snapshot(),
                    ingest: state.ingest_metrics.snapshot(),
                };
                (
                    StatusCode::OK,
                    Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
                )
            }
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            ),
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

// ---------------------------------------------------------------------
// open-sessions
// ---------------------------------------------------------------------

/// Query string for `GET /admin/open-sessions` — open (not yet ended)
/// sessions for one scope + agent, newest first. Backs the thin
/// `ai-memory finalize-session` command, which posts synthetic
/// session-end hooks for whatever this returns.
#[derive(Debug, Deserialize)]
struct OpenSessionsQuery {
    /// Workspace name (required).
    workspace: String,
    /// Project name (required).
    project: String,
    /// Agent kind filter, kebab-case (`codex`, `claude-code`, …).
    agent: String,
    /// When true, return every open session; otherwise just the newest.
    #[serde(default)]
    all: bool,
    /// Include sessions belonging to OTHER operators.
    ///
    /// Off by default because the caller of this endpoint (`finalize-session`)
    /// acts destructively on what it returns: it ends the session, synthesises
    /// a page from its observations and mints a handoff carrying its raw
    /// prompts. Picking "the newest open session in the scope" across everyone
    /// would do all of that to a colleague's live session. Sessions with no
    /// recorded operator stay visible either way, so a single-operator server
    /// is unaffected.
    #[serde(default)]
    all_owners: bool,
    /// When set, narrow to exactly this session id (still subject to the
    /// owner filter above) instead of newest-first selection — lets a
    /// caller that captured its own session id at spawn time finalize
    /// precisely that session, even when other sessions for the same agent
    /// are open concurrently in the same project (e.g. multiple terminal
    /// tabs each running Kiro CLI against one repo).
    #[serde(default)]
    session_id: Option<SessionId>,
}

/// Wire shape for one open session in the `GET /admin/open-sessions`
/// response.
#[derive(Debug, Serialize)]
struct OpenSessionEntry {
    session_id: String,
    cwd: Option<String>,
}

/// Query string for `GET /admin/activity/by-client` — MCP tool-call
/// counts per client. Server-wide: MCP-only clients (the reason this
/// exists) are not reliably scoped to one project per call, and the
/// endpoint mirrors the buffer's own granularity.
///
/// `since_days = 0` means the whole history, mirroring
/// `/admin/sessions/by-agent`.
#[derive(Debug, Deserialize)]
struct ActivityByClientQuery {
    #[serde(default)]
    since_days: u32,
}

async fn handle_activity_by_client(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<ActivityByClientQuery>,
) -> impl IntoResponse {
    let since_day = (query.since_days > 0).then(|| {
        jiff::Timestamp::now()
            .as_microsecond()
            .saturating_sub(i64::from(query.since_days).saturating_mul(US_PER_DAY))
            .div_euclid(US_PER_DAY)
    });
    match state.reader.client_activity_since(since_day).await {
        Ok(rows) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(&rows)
                    .map(|list| serde_json::json!({ "by_client": list }))
                    .unwrap_or_else(|_| serde_json::json!({ "by_client": [] })),
            ),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// Query string for `GET /admin/sessions/by-agent` — how many sessions each
/// agent CLI opened in one scope.
///
/// `since_days = 0` means "no lower bound": count the project's whole
/// history rather than an empty window, which is what a dashboard asking for
/// "all time" wants and what a `u32` cannot express as `None`.
#[derive(Debug, Deserialize)]
struct SessionsByAgentQuery {
    /// Workspace name (required).
    workspace: String,
    /// Project name (required).
    project: String,
    /// Inclusive lookback in days; zero means all history.
    #[serde(default)]
    since_days: u32,
    /// Report every operator's sessions instead of just the caller's. Same
    /// recovery switch the other scoped admin reads expose.
    #[serde(default)]
    all_owners: bool,
}

/// Microseconds in a day, for the `since_days` window.
const US_PER_DAY: i64 = 86_400_000_000;

async fn handle_sessions_by_agent(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Query(query): Query<SessionsByAgentQuery>,
) -> impl IntoResponse {
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let since_us = (query.since_days > 0).then(|| {
        jiff::Timestamp::now()
            .as_microsecond()
            .saturating_sub(i64::from(query.since_days).saturating_mul(US_PER_DAY))
    });
    let owner_filter = if query.all_owners {
        ai_memory_core::OwnerFilter::Any
    } else {
        ai_memory_core::OwnerFilter::for_actor_context(
            &actor_ext.map_or_else(ai_memory_core::ActorContext::anonymous, |ext| ext.0),
        )
    };
    let counts: ai_memory_store::StoreResult<Vec<ai_memory_store::AgentSessionCount>> = state
        .reader
        .session_counts_by_agent(ws, proj, owner_filter, since_us)
        .await;
    match counts {
        Ok(counts) => (
            StatusCode::OK,
            Json(
                serde_json::to_value(&counts)
                    .map(|list| serde_json::json!({ "by_agent": list }))
                    .unwrap_or_else(|_| serde_json::json!({ "by_agent": [] })),
            ),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// Parse a kebab-case agent wire string into an [`AgentKind`].
fn parse_agent_kind(raw: &str) -> Option<AgentKind> {
    AgentKind::ALL.into_iter().find(|kind| kind.as_str() == raw)
}

async fn handle_open_sessions(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Query(query): Query<OpenSessionsQuery>,
) -> impl IntoResponse {
    if query.all && query.session_id.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "all and session_id cannot be combined"
            })),
        );
    }
    let Some(agent) = parse_agent_kind(&query.agent) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("unknown agent kind: {}", query.agent)
            })),
        );
    };
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let owner_filter = if query.all_owners {
        ai_memory_core::OwnerFilter::Any
    } else {
        ai_memory_core::OwnerFilter::for_actor_context(
            &actor_ext.map_or_else(ai_memory_core::ActorContext::anonymous, |ext| ext.0),
        )
    };
    let sessions = if let Some(session_id) = query.session_id {
        state
            .reader
            .open_session_for_scope_agent_by_id(ws, proj, agent, owner_filter, session_id)
            .await
            .map(|session| session.into_iter().collect())
    } else {
        let limit = if query.all { None } else { Some(1) };
        state
            .reader
            .open_sessions_for_scope_agent(ws, proj, agent, owner_filter, limit)
            .await
    };
    match sessions {
        Ok(sessions) => {
            let sessions: Vec<OpenSessionEntry> = sessions
                .into_iter()
                .map(|s| OpenSessionEntry {
                    session_id: s.session_id.to_string(),
                    cwd: s.cwd,
                })
                .collect();
            (
                StatusCode::OK,
                Json(
                    serde_json::to_value(&sessions)
                        .map(|list| serde_json::json!({ "sessions": list }))
                        .unwrap_or_else(|_| serde_json::json!({ "sessions": [] })),
                ),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

// ---------------------------------------------------------------------
// search
// ---------------------------------------------------------------------

/// Query string for `GET /admin/search?q=…&limit=…`.
#[derive(Debug, Deserialize)]
struct SearchQuery {
    /// FTS5 query expression.
    q: String,
    /// Workspace name to search within. Must be provided together with `project`.
    #[serde(default)]
    workspace: Option<String>,
    /// Project name to search within. Must be provided together with `workspace`.
    #[serde(default)]
    project: Option<String>,
    /// Max number of hits to return. Capped at 100 server-side.
    #[serde(default = "default_search_limit")]
    limit: usize,
}

fn default_search_limit() -> usize {
    10
}

async fn handle_search(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 100);
    let search_result = match (
        trimmed_opt(query.workspace.as_deref()),
        trimmed_opt(query.project.as_deref()),
    ) {
        (Some(workspace), Some(project)) => {
            match lookup_ws_proj_no_create(&state, workspace, project).await {
                Ok((ws, proj)) => {
                    state
                        .reader
                        .search_pages_for_project(ws, proj, query.q, limit, None)
                        .await
                }
                Err(e) => return e,
            }
        }
        (Some(_), None) | (None, Some(_)) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "workspace and project must be provided together"
                })),
            );
        }
        _ => state.reader.search_pages(query.q, limit).await,
    };
    match search_result {
        Ok(hits) => (
            StatusCode::OK,
            Json(serde_json::to_value(&hits).unwrap_or_else(|_| serde_json::json!([]))),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

// ---------------------------------------------------------------------
// read-page
// ---------------------------------------------------------------------

/// Query string for `GET /admin/read-page`.
///
/// Two modes:
/// - Path mode: `workspace` + `project` + `path` — direct lookup.
/// - Query mode: `workspace` + `project` + `q` — FTS5 search scoped to
///   the given project; fetches the top-ranking hit's full body.
#[derive(Debug, Deserialize)]
struct ReadPageQuery {
    /// Workspace name (required).
    workspace: String,
    /// Project name (required).
    project: String,
    /// Direct wiki path (e.g. `notes/foo.md`). Takes precedence over `q`.
    #[serde(default)]
    path: Option<String>,
    /// FTS5 query. Used when `path` is absent; fetches the top hit's full body.
    #[serde(default)]
    q: Option<String>,
}

/// Response body for `GET /admin/read-page`.
#[derive(Debug, Serialize)]
struct ReadPageResponse {
    path: String,
    workspace: String,
    project: String,
    title: Option<String>,
    body: String,
    frontmatter: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    served_from: Option<&'static str>,
}

async fn handle_read_page(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<ReadPageQuery>,
) -> impl IntoResponse {
    let direct_path = if let Some(raw) = query.path {
        match ai_memory_core::PagePath::new(raw) {
            Ok(p) => Some(p),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": format!("invalid path: {e}") })),
                );
            }
        }
    } else {
        None
    };
    if direct_path.is_none() && query.q.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "provide `path` or `q`" })),
        );
    }

    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };

    // Resolve the page path: direct `path` takes precedence over `q`.
    let page_path = if let Some(path) = direct_path {
        path
    } else if let Some(q) = query.q {
        let hits = match state
            .reader
            .search_pages_for_project(ws, proj, q.clone(), 1, None)
            .await
        {
            Ok(h) => h,
            Err(e) => return internal_err(e.to_string()),
        };
        match hits.into_iter().next() {
            Some(h) => h.path,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": format!("no pages found for query {q:?}") })),
                );
            }
        }
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "provide `path` or `q`" })),
        );
    };

    match state.wiki.read_page(ws, proj, &page_path) {
        Ok(md) => {
            let title = md
                .frontmatter
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let resp = ReadPageResponse {
                path: page_path.to_string(),
                workspace: query.workspace,
                project: query.project,
                title,
                body: md.body,
                frontmatter: md.frontmatter,
                served_from: None,
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(&resp).unwrap_or_else(|_| serde_json::json!({}))),
            )
        }
        // Only a missing markdown file can fall back to the DB copy. Other disk
        // errors belong to the source-of-truth file and must be surfaced.
        Err(disk_err) if is_missing_wiki_file(&disk_err) => match state
            .reader
            .page_body_by_ids(ws, proj, page_path.as_str())
            .await
        {
            Ok(Some(stored)) => {
                let frontmatter: serde_json::Value = serde_json::from_str(&stored.frontmatter_json)
                    .unwrap_or(serde_json::Value::Null);
                let title = frontmatter
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .or(Some(stored.title));
                let resp = ReadPageResponse {
                    path: page_path.to_string(),
                    workspace: query.workspace,
                    project: query.project,
                    title,
                    body: stored.body,
                    frontmatter,
                    served_from: Some("db-fallback"),
                };
                (
                    StatusCode::OK,
                    Json(serde_json::to_value(&resp).unwrap_or_else(|_| serde_json::json!({}))),
                )
            }
            Ok(None) => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": disk_err.to_string() })),
            ),
            Err(e) => internal_err(e.to_string()),
        },
        Err(disk_err) => internal_err(disk_err.to_string()),
    }
}

// ---------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------

/// Build a 500 response carrying the given message.
fn internal_err(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": msg.into() })),
    )
}

fn is_missing_wiki_file(err: &WikiError) -> bool {
    matches!(err, WikiError::Io(e) if e.kind() == std::io::ErrorKind::NotFound)
}

fn trimmed_opt(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|s| !s.is_empty())
}

/// Resolve workspace + project IDs, creating them if absent. Returns
/// either the IDs or a ready-to-return error response.
async fn create_ws_proj(
    state: &AdminState,
    workspace: &str,
    project: &str,
) -> Result<(WorkspaceId, ProjectId), (StatusCode, Json<serde_json::Value>)> {
    create_explicit_scope(&state.writer, workspace, project)
        .await
        .map(ai_memory_store::ResolvedScope::as_tuple)
        .map_err(scope_err)
}

/// Look up workspace + project by name **without** auto-creating them.
/// Returns `(WorkspaceId, ProjectId)` on success, or a ready-to-return
/// 404/500 error response. Used by read, maintenance, and destructive
/// handlers where auto-creation would silently succeed on a typo.
async fn lookup_ws_proj_no_create(
    state: &AdminState,
    workspace: &str,
    project: &str,
) -> Result<(WorkspaceId, ProjectId), (StatusCode, Json<serde_json::Value>)> {
    lookup_existing_scope(&state.reader, workspace, project)
        .await
        .map(ai_memory_store::ResolvedScope::as_tuple)
        .map_err(scope_err)
}

/// Look up a workspace by name **without** auto-creating it.
async fn lookup_ws_no_create(
    state: &AdminState,
    workspace: &str,
) -> Result<WorkspaceId, (StatusCode, Json<serde_json::Value>)> {
    lookup_existing_workspace(&state.reader, workspace)
        .await
        .map_err(scope_err)
}

fn scope_err(err: ScopeResolutionError) -> (StatusCode, Json<serde_json::Value>) {
    let status = if err.is_bad_request() {
        StatusCode::BAD_REQUEST
    } else if err.is_not_found() {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        status,
        Json(serde_json::json!({ "error": err.to_string() })),
    )
}

fn skip_webhooks_for_admin_request(
    level_ext: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    headers: &HeaderMap,
) -> Vec<String> {
    let level = level_ext
        .map(|axum::Extension(level)| level)
        .unwrap_or(ai_memory_core::AuthLevel::Anonymous);
    if level
        .authorize(Capability::SkipAdmissionChain, true)
        .is_ok()
    {
        crate::actor::skip_webhooks_from_headers(headers)
    } else {
        Vec::new()
    }
}

// ---------------------------------------------------------------------
// bootstrap
// ---------------------------------------------------------------------

async fn handle_bootstrap(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<BootstrapRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // Dry-runs are READ-ONLY: no LLM call, no project creation, no
    // wiki write, no git commit. Early-return BEFORE any state-touching
    // call so a smoke-test (e.g. `--dry-run` from a tempdir) cannot
    // pollute the project list with throwaway names. Dry-runs can also
    // proceed in parallel with anything else — no mutex needed.
    if req.dry_run {
        return dry_run_outcome(
            req.sources,
            req.sources_collected,
            req.max_input_tokens,
            req.chunk_input_tokens,
        );
    }

    // Live runs from here on need LLM + workspace/project resolution.
    if state.llm.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "LLM provider not configured on server"
            })),
        ));
    }
    let (ws, proj) = create_ws_proj(&state, &req.workspace, &req.project).await?;

    // Serialise live bootstrap runs. Two parallel `process_sources`
    // calls would race the wiki's `commit_all` (libgit2 ops on the
    // same repo) and stack LLM cost unnecessarily. The wait is
    // operator-visible only in log lines; the request stays open
    // until the lock is acquired, then proceeds normally.
    if state.bootstrap_lock.try_lock().is_err() {
        info!(
            "another bootstrap is in progress; queueing — \
             this request waits for the active one to finish"
        );
    }
    let _bootstrap_guard = state.bootstrap_lock.lock().await;

    let llm = Arc::clone(
        state
            .llm
            .as_ref()
            .expect("llm is Some: checked above (non-dry-run without LLM returns 503)"),
    );

    let cfg = BootstrapConfig {
        // repo_path is unused by process_sources — the path field is
        // only consumed by collect_sources on the client side.
        repo_path: std::path::PathBuf::new(),
        workspace_id: ws,
        project_id: proj,
        max_input_tokens: req.max_input_tokens,
        chunk_input_tokens: req.chunk_input_tokens,
        sources_collected: req.sources_collected,
        // The individual include_* flags don't matter here: sources
        // are already collected; process_sources ignores them.
        include_git: true,
        include_readme: true,
        include_docs: true,
        include_code: true,
        since: None,
        dry_run: req.dry_run,
        force: req.force,
    };

    let bootstrap = Bootstrap {
        reader: state.reader.clone(),
        wiki: state.wiki.clone(),
        llm,
    };

    match bootstrap.process_sources(&cfg, req.sources).await {
        Ok(outcome) => Ok((
            StatusCode::OK,
            Json(serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({}))),
        )),
        Err(e) => Err(bootstrap_error_response(e)),
    }
}

/// Build a dry-run [`BootstrapOutcome`] without an LLM by applying the
/// same budget-pruning logic that `Bootstrap::process_sources` would use.
/// Map a [`BootstrapError`] to the appropriate HTTP status code.
///
/// - `NoSources` / `AlreadyBootstrapped` → 422 (validation failures).
/// - `Llm` → 502 (upstream provider failure).
/// - Everything else → 500 (unexpected server-side error).
fn bootstrap_error_response(
    e: ai_memory_consolidate::BootstrapError,
) -> (StatusCode, Json<serde_json::Value>) {
    use ai_memory_consolidate::BootstrapError;
    let status = match &e {
        BootstrapError::NoSources | BootstrapError::AlreadyBootstrapped => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        BootstrapError::Llm(_) => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(serde_json::json!({ "error": e.to_string() })))
}

/// Build a dry-run [`BootstrapOutcome`] without an LLM by applying the
/// same budget-pruning logic that `Bootstrap::process_sources` would use.
fn dry_run_outcome(
    sources: Vec<BootstrapSource>,
    sources_collected: Option<usize>,
    max_input_tokens: usize,
    chunk_input_tokens: usize,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    use ai_memory_consolidate::BootstrapError;
    if sources.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": BootstrapError::NoSources.to_string()
            })),
        ));
    }
    let incoming = sources.len();
    let (kept, _dropped, total) = prune_sources_to_budget(sources, max_input_tokens);
    if kept.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": BootstrapError::NoSources.to_string()
            })),
        ));
    }
    let collected = sources_collected.unwrap_or(incoming);
    let sources_sent = kept.len();
    let sources_dropped = collected.saturating_sub(sources_sent);
    let counts = SourceCounts::from_sources(&kept);
    let chunk_budget =
        ai_memory_consolidate::effective_chunk_budget(chunk_input_tokens, max_input_tokens);
    let llm_chunks = ai_memory_consolidate::plan_bootstrap_chunks(kept.clone(), chunk_budget).len();
    let outcome = BootstrapOutcome {
        sources_collected: collected,
        sources_sent,
        sources_dropped,
        sources_by_kind: counts,
        estimated_input_tokens: total,
        pages_written: Vec::new(),
        rationale: "(dry-run; LLM not invoked)".to_string(),
        dry_run: true,
        llm_chunks,
    };
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&outcome).unwrap_or_else(|_| serde_json::json!({}))),
    ))
}

// ---------------------------------------------------------------------
// auto-improve reviewer/stager
// ---------------------------------------------------------------------

async fn handle_auto_improve(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    author_ext: Option<axum::Extension<ai_memory_core::UserId>>,
    level_ext: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    headers: HeaderMap,
    Json(req): Json<AutoImproveRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if req.dry_run.is_some() || req.stage.is_some() || req.mode.is_some() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "auto-improve dry_run/stage/mode request fields were removed; set [auto_improve].require_approval = true for manual review"
            })),
        ));
    }
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;
    let Some(llm) = state.llm.as_ref().cloned() else {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "LLM provider not configured on server"
            })),
        ));
    };

    let cfg = AutoImproveReviewConfig {
        min_observations: req.min_observations,
        min_session_duration_secs: req.min_session_duration_secs,
        min_confidence: req.min_confidence,
        max_input_tokens: req.max_input_tokens,
        max_proposals_per_run: req.max_proposals_per_run,
        include_raw_fallback: req.include_raw_fallback,
        proposal_actor: req.proposal_actor.clone(),
        pending_path: req.pending_path.clone(),
        max_patchable_pages: req.max_patchable_pages,
        max_patchable_body_chars: req.max_patchable_body_chars,
        max_edits_per_proposal: req.max_edits_per_proposal,
        max_edit_content_chars: req.max_edit_content_chars,
        max_changed_chars_per_proposal: req.max_changed_chars_per_proposal,
        max_patch_edits_per_run: req.max_patch_edits_per_run,
        max_rejection_context: req.max_rejection_context,
        rejection_context_days: req.rejection_context_days,
        max_final_body_chars: req.max_final_body_chars,
        max_rule_page_tokens: req.max_rule_page_tokens,
        max_procedure_page_tokens: req.max_procedure_page_tokens,
        eval: req
            .eval
            .clone()
            .unwrap_or_else(|| state.auto_improve_review_config.eval.clone()),
    };

    let report =
        run_auto_improve_review(&state.reader, &*llm, ws, proj, req.session_id, cfg.clone())
            .await
            .map_err(auto_improve_error_response)?;
    // Whose suggestion this is. It scopes the one-pending-per-target rule
    // (V42), and is taken from the authenticated actor's qualified identity
    // key rather than a `users` row, because most identified operators on a
    // shared server are named by an authenticating proxy and never get one.
    //
    // Only where operators are actually told apart, though: on a
    // single-operator server this call would otherwise stage into bucket
    // `user:<root_username>` while the scheduler and the report handlers stage
    // into the unattributed one, leaving two proposals pending for the same
    // page — the collision V42 promises cannot happen.
    //
    // Both halves go through the shared accessors — `identity_key` for "which
    // human is this", `owner_identity` for "does this deployment name them" —
    // rather than re-deriving either here. Its `memory_auto_improve` sibling
    // stages into the same V42 bucket, and a bucket computed two ways is a
    // bucket that eventually disagrees with itself.
    let staging_owner = ai_memory_core::owner_identity(
        actor_ext
            .as_ref()
            .and_then(|axum::Extension(actor)| actor.identity_key())
            .as_ref(),
        state
            .reader
            .distinguishes_operators(state.trusted_proxy_identity)
            .await
            .map_err(|e| internal_err(e.to_string()))?,
    );
    let proposals = auto_improve_new_proposals(&state, ws, proj, &report).await?;
    let staged = stage_auto_improve_report(
        &state,
        AutoImproveStagingScope {
            ws,
            proj,
            session_id: req.session_id,
            cfg: &cfg,
            staging_owner,
        },
        &report,
        proposals,
    )
    .await?;
    let actor = actor_ext
        .map(|axum::Extension(actor)| actor)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    let author_id = author_ext.map(|axum::Extension(author_id)| author_id);
    let skip_webhooks = skip_webhooks_for_admin_request(level_ext, &headers);
    let outcomes = finalize_auto_improve_proposals(
        &state,
        &staged,
        AutoImproveFinalizeContext {
            workspace_id: ws,
            project_id: proj,
            require_approval: state.auto_improve_require_approval,
            actor,
            author_id,
            admission_ctx: Some(AdmissionContext {
                op: AdmissionOp::WritePage,
                skip_webhooks,
                ..AdmissionContext::default()
            }),
        },
    )
    .await?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(AutoImproveStageResponse {
                run_id: staged.run_id.to_string(),
                approval_required: state.auto_improve_require_approval,
                approval_policy: if state.auto_improve_require_approval {
                    "manual".into()
                } else {
                    "auto_approve".into()
                },
                session_id: req.session_id.to_string(),
                summary: report.summary,
                warnings: report.warnings,
                rejected_candidates_count: report.rejected_candidates.len(),
                proposals: outcomes,
                // Store-level collisions the reviewer's run never staged.
                // Dropped silently, a run reporting N-1 proposals has nothing
                // saying the Nth ever existed.
                skipped: staged.skipped,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
    ))
}

fn auto_improve_auto_approve_actor(
    mut actor: ai_memory_core::ActorContext,
) -> ai_memory_core::ActorContext {
    actor.agent = Some("auto_improve_auto_approve".into());
    actor
}

struct AutoImproveFinalizeContext {
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    require_approval: bool,
    actor: ai_memory_core::ActorContext,
    author_id: Option<ai_memory_core::UserId>,
    admission_ctx: Option<AdmissionContext>,
}

async fn finalize_auto_improve_proposals(
    state: &AdminState,
    staged: &StagedAutoImproveData,
    ctx: AutoImproveFinalizeContext,
) -> Result<Vec<AutoImproveProposalOutcome>, (StatusCode, Json<serde_json::Value>)> {
    let mut outcomes = Vec::with_capacity(staged.proposal_ids.len());
    for (proposal_id, sidecar_path) in staged.proposal_ids.iter().zip(staged.sidecar_paths.iter()) {
        if ctx.require_approval {
            outcomes.push(AutoImproveProposalOutcome {
                id: proposal_id.to_string(),
                sidecar_path: sidecar_path.clone(),
                status: "pending".into(),
                page_id: None,
            });
            continue;
        }

        let approval_actor = auto_improve_auto_approve_actor(ctx.actor.clone());
        match state
            .wiki
            .approve_auto_improve_proposal(
                ctx.workspace_id,
                ctx.project_id,
                *proposal_id,
                approval_actor,
                ctx.author_id,
                ctx.admission_ctx.clone(),
            )
            .await
        {
            Ok(ApproveAutoImproveProposalResult::Approved { page_id }) => {
                outcomes.push(AutoImproveProposalOutcome {
                    id: proposal_id.to_string(),
                    sidecar_path: sidecar_path.clone(),
                    status: "approved".into(),
                    page_id: Some(page_id.to_string()),
                });
            }
            Ok(ApproveAutoImproveProposalResult::Conflict) => {
                outcomes.push(AutoImproveProposalOutcome {
                    id: proposal_id.to_string(),
                    sidecar_path: sidecar_path.clone(),
                    status: "conflict".into(),
                    page_id: None,
                });
            }
            Err(e) => return Err(internal_err(e.to_string())),
        }
    }
    Ok(outcomes)
}

async fn auto_improve_new_proposals(
    state: &AdminState,
    ws: WorkspaceId,
    proj: ProjectId,
    report: &ai_memory_consolidate::AutoImproveReport,
) -> Result<Vec<NewAutoImproveProposal>, (StatusCode, Json<serde_json::Value>)> {
    let mut proposals = Vec::with_capacity(report.proposals.len());
    for p in &report.proposals {
        let path = PagePath::new(p.path.clone()).map_err(|e| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({ "error": format!("invalid proposal path: {e}") })),
            )
        })?;
        let target_exists = state
            .reader
            .page_body_by_ids(ws, proj, path.as_str())
            .await
            .map_err(|e| internal_err(e.to_string()))?
            .is_some();
        let operation = if p.edit_mode == "patch"
            || (target_exists && path.as_str() == "_slots/current-focus.md")
        {
            AutoImproveProposalOperation::Update
        } else {
            AutoImproveProposalOperation::Create
        };
        let expected_base_body_sha256 = p
            .expected_base_body_sha256
            .as_deref()
            .map(hex_to_sha256)
            .transpose()
            .map_err(|e| internal_err(format!("invalid expected_base_body_sha256: {e}")))?;
        proposals.push(NewAutoImproveProposal {
            operation,
            target_path: path,
            kind: p.kind.clone(),
            title: p.title.clone(),
            confidence: f64::from(p.confidence),
            rationale: p.rationale.clone(),
            evidence_json: serde_json::to_value(&p.evidence)
                .map_err(|e| internal_err(e.to_string()))?,
            body_markdown: p.body_markdown.clone(),
            artifact_sha256: None,
            edit_mode: Some(p.edit_mode.clone()),
            patch_json: serde_json::to_value(&p.edits).ok(),
            expected_base_body_sha256,
        });
    }
    Ok(proposals)
}

struct StagedAutoImproveData {
    run_id: ai_memory_core::AutoImproveRunId,
    proposal_ids: Vec<AutoImproveProposalId>,
    sidecar_paths: Vec<String>,
    /// Proposals the store refused to stage. Dropping these here would hand the
    /// operator a run reporting N-1 proposals with nothing saying the Nth
    /// existed — the silent drop the per-proposal skip was meant to end.
    skipped: Vec<SkippedProposal>,
}

/// Everything `stage_auto_improve_report` needs about WHO and WHERE, bundled so
/// the helper keeps a readable arity.
struct AutoImproveStagingScope<'a> {
    ws: WorkspaceId,
    proj: ProjectId,
    session_id: SessionId,
    cfg: &'a AutoImproveReviewConfig,
    /// Typed operator that staged the run; also scopes the
    /// one-pending-per-target rule.
    staging_owner: Option<ai_memory_core::IdentityKey>,
}

async fn stage_auto_improve_report(
    state: &AdminState,
    scope: AutoImproveStagingScope<'_>,
    report: &ai_memory_consolidate::AutoImproveReport,
    proposals: Vec<NewAutoImproveProposal>,
) -> Result<StagedAutoImproveData, (StatusCode, Json<serde_json::Value>)> {
    let AutoImproveStagingScope {
        ws,
        proj,
        session_id,
        cfg,
        staging_owner,
    } = scope;
    let staged = state
        .writer
        .stage_auto_improve_run_for_owner(
            StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: Some(session_id),
                provider: Some(report.provider.clone()),
                model: Some(report.model.clone()),
                summary: Some(report.summary.clone()),
                warnings_json: serde_json::to_value(&report.warnings)
                    .unwrap_or_else(|_| serde_json::json!([])),
                rejected_candidates_json: serde_json::to_value(&report.rejected_candidates)
                    .unwrap_or_else(|_| serde_json::json!([])),
                config_json: serde_json::json!({
                    "min_observations": cfg.min_observations,
                    "min_session_duration_secs": cfg.min_session_duration_secs,
                    "min_confidence": cfg.min_confidence,
                    "max_input_tokens": cfg.max_input_tokens,
                    "max_proposals_per_run": cfg.max_proposals_per_run,
                    "include_raw_fallback": cfg.include_raw_fallback,
                    "max_patchable_pages": cfg.max_patchable_pages,
                    "max_patchable_body_chars": cfg.max_patchable_body_chars,
                    "max_edits_per_proposal": cfg.max_edits_per_proposal,
                    "max_edit_content_chars": cfg.max_edit_content_chars,
                    "max_changed_chars_per_proposal": cfg.max_changed_chars_per_proposal,
                    "max_patch_edits_per_run": cfg.max_patch_edits_per_run,
                    "max_rejection_context": cfg.max_rejection_context,
                    "rejection_context_days": cfg.rejection_context_days,
                    "max_final_body_chars": cfg.max_final_body_chars,
                    "max_rule_page_tokens": cfg.max_rule_page_tokens,
                    "max_procedure_page_tokens": cfg.max_procedure_page_tokens,
                    "eval": cfg.eval,
                }),
                proposal_actor: ai_memory_core::ActorContext {
                    agent: Some(cfg.proposal_actor.clone()),
                    ..ai_memory_core::ActorContext::default()
                },
                proposals,
            },
            staging_owner,
        )
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let sidecar_paths = write_auto_improve_sidecars(state, ws, proj, &staged.proposal_ids).await?;
    Ok(StagedAutoImproveData {
        run_id: staged.run_id,
        proposal_ids: staged.proposal_ids,
        sidecar_paths,
        skipped: staged.skipped,
    })
}

async fn target_operation_for_page(
    state: &AdminState,
    ws: WorkspaceId,
    proj: ProjectId,
    target: &PagePath,
) -> Result<AutoImproveProposalOperation, (StatusCode, Json<serde_json::Value>)> {
    if state
        .reader
        .page_body_by_ids(ws, proj, target.as_str())
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .is_some()
    {
        Ok(AutoImproveProposalOperation::Update)
    } else {
        Ok(AutoImproveProposalOperation::Create)
    }
}

async fn write_auto_improve_sidecars(
    state: &AdminState,
    ws: WorkspaceId,
    proj: ProjectId,
    proposal_ids: &[AutoImproveProposalId],
) -> Result<Vec<String>, (StatusCode, Json<serde_json::Value>)> {
    let mut sidecar_paths = Vec::with_capacity(proposal_ids.len());
    for id in proposal_ids {
        let path = state
            .wiki
            .write_auto_improve_sidecar(ws, proj, *id)
            .await
            .map_err(|e| internal_err(e.to_string()))?;
        sidecar_paths.push(path.display().to_string());
    }
    Ok(sidecar_paths)
}

fn auto_improve_error_response(
    e: ai_memory_consolidate::AutoImproveError,
) -> (StatusCode, Json<serde_json::Value>) {
    use ai_memory_consolidate::AutoImproveError;
    let status = match &e {
        AutoImproveError::SessionNotFound(_) => StatusCode::NOT_FOUND,
        AutoImproveError::SessionOutOfScope { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        AutoImproveError::Llm(_) => StatusCode::BAD_GATEWAY,
        AutoImproveError::Eval(_) => StatusCode::BAD_GATEWAY,
        AutoImproveError::Memory(_) => StatusCode::BAD_REQUEST,
        AutoImproveError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(serde_json::json!({ "error": e.to_string() })))
}

// ---------------------------------------------------------------------
// auto-improve telemetry report
// ---------------------------------------------------------------------

async fn handle_auto_improve_report(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<AutoImproveTelemetryReportRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;
    let params = AutoImproveTelemetryParams {
        since_days: req.since_days,
        top_limit: req.limit.clamp(1, 100),
    };
    let report = run_auto_improve_telemetry_report(
        &state.reader,
        ws,
        proj,
        &req.workspace,
        &req.project,
        params.clone(),
    )
    .await
    .map_err(|e| internal_err(e.to_string()))?;
    if req.stage {
        let body_markdown = render_auto_improve_telemetry_report_markdown(&report);
        let target_path = auto_improve_report_target_path();
        let target = PagePath::new(target_path).map_err(|e| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({ "error": format!("invalid auto-improve report target path: {e}") })),
            )
        })?;
        let operation = target_operation_for_page(&state, ws, proj, &target).await?;
        let staged = state
            .writer
            .stage_auto_improve_run_for_owner(
                StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: None,
                provider: None,
                model: None,
                summary: Some(format!("Auto-improve telemetry report: {}", report.summary)),
                warnings_json: serde_json::json!([]),
                rejected_candidates_json: serde_json::json!([]),
                config_json: serde_json::json!({
                    "mode": "stage",
                    "auto_improve_report": true,
                    "params": params,
                }),
                proposal_actor: ai_memory_core::ActorContext {
                    agent: Some("auto_improve_report".into()),
                    ..ai_memory_core::ActorContext::default()
                },
                proposals: vec![NewAutoImproveProposal {
                    operation,
                    target_path: target,
                    kind: "auto_improve_report".into(),
                    title: "Auto-improve Telemetry Report".into(),
                    confidence: 1.0,
                    rationale: "Telemetry report only; approval writes the report page and performs no learning-memory changes.".into(),
                    evidence_json: serde_json::json!({
                        "summary": report.summary.clone(),
                        "findings": report.findings.clone(),
                        "params": report.params.clone(),
                    }),
                    body_markdown,
                    artifact_sha256: None,
                    edit_mode: None,
                    patch_json: None,
                    expected_base_body_sha256: None,
                }],
                },
                None,
            )
            .await
            .map_err(|e| internal_err(e.to_string()))?;
        let sidecar_paths =
            write_auto_improve_sidecars(&state, ws, proj, &staged.proposal_ids).await?;
        return Ok((
            StatusCode::OK,
            Json(
                serde_json::to_value(AutoImproveTelemetryReportStageResponse {
                    run_id: staged.run_id.to_string(),
                    proposal_ids: staged
                        .proposal_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    sidecar_paths,
                    skipped: staged.skipped,
                    report,
                })
                .unwrap_or_else(|_| serde_json::json!({})),
            ),
        ));
    }
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    ))
}

async fn handle_curator(
    State(state): State<Arc<AdminState>>,
    axum::Extension(tuning): axum::Extension<SweepTuning>,
    Json(req): Json<CuratorRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let mode = req.mode.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let wants_stage = req.stage || matches!(mode, Some("stage"));
    let explicit_dry_run = req.dry_run || matches!(mode, Some("dry_run" | "dry-run"));
    if wants_stage && explicit_dry_run {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": "choose either dry_run or stage, not both" })),
        ));
    }
    if let Some(other) = mode
        && !matches!(other, "stage" | "dry_run" | "dry-run")
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": format!("unknown curator mode '{other}'") })),
        ));
    }
    let wants_dry_run = explicit_dry_run || !wants_stage;

    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;
    let params = CuratorParams::default();
    let mut report = run_curator_report_with_breadth(
        &state.reader,
        ws,
        proj,
        &req.workspace,
        &req.project,
        params.clone(),
        tuning.breadth_weight,
    )
    .await
    .map_err(|e| internal_err(e.to_string()))?;
    if wants_dry_run {
        report.dry_run = true;
        return Ok((
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        ));
    }

    report.dry_run = false;
    let body_markdown = render_curator_report_markdown(&report);
    let target_path = curator_target_path();
    let target = PagePath::new(target_path).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": format!("invalid curator target path: {e}") })),
        )
    })?;
    let operation = target_operation_for_page(&state, ws, proj, &target).await?;

    let staged = state
            .writer
            .stage_auto_improve_run_for_owner(
                StageAutoImproveRun {
                    workspace_id: ws,
                    project_id: proj,
                    session_id: None,
                    provider: None,
                    model: None,
                    summary: Some(report.summary.clone()),
                    warnings_json: serde_json::json!([]),
                    rejected_candidates_json: serde_json::json!([]),
                    config_json: serde_json::json!({
                        "mode": "stage",
                        "curator": true,
                        "params": params,
                    }),
                    proposal_actor: ai_memory_core::ActorContext {
                        agent: Some("curator".into()),
                        ..ai_memory_core::ActorContext::default()
                    },
                    proposals: vec![NewAutoImproveProposal {
                        operation,
                        target_path: target,
                        kind: "curator_report".into(),
                        title: "Curator Report".into(),
                        confidence: 1.0,
                        rationale: "Rule-based curator report only; approval writes the report page and performs no maintenance actions.".into(),
                        evidence_json: serde_json::json!({
                            "summary": report.summary.clone(),
                            "findings": report.findings.clone(),
                        }),
                        body_markdown,
                        artifact_sha256: None,
                        edit_mode: None,
                        patch_json: None,
                        expected_base_body_sha256: None,
                    }],
                },
            None,
        )
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let sidecar_paths = write_auto_improve_sidecars(&state, ws, proj, &staged.proposal_ids).await?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(CuratorStageResponse {
                run_id: staged.run_id.to_string(),
                proposal_ids: staged
                    .proposal_ids
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                sidecar_paths,
                skipped: staged.skipped,
                report,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
    ))
}

fn curator_target_path() -> String {
    let now = jiff::Timestamp::now().to_string();
    let date = now.get(0..10).unwrap_or("latest");
    format!("notes/curator-{date}.md")
}

fn auto_improve_report_target_path() -> String {
    let stamp = jiff::Timestamp::now().as_microsecond();
    format!("notes/auto-improve-report-{stamp}.md")
}

/// List open handoffs so an operator can cancel one (#513).
///
/// `memory_handoff_cancel` requires an exact id and nothing exposed one, so a
/// backlog showed up as a count in `status` and could not be addressed.
/// Read-only, oldest first, and content-free: identity, provenance and age,
/// never the handoff body.
async fn handle_open_handoffs_list(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<OpenHandoffsQuery>,
) -> impl IntoResponse {
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let limit = query.limit.clamp(1, 500);
    match state
        .reader
        .open_handoffs_for_project(ws, proj, limit)
        .await
    {
        Ok(handoffs) => (
            StatusCode::OK,
            Json(serde_json::json!({ "handoffs": handoffs })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

/// `POST /admin/handoffs/expire` request body.
#[derive(Deserialize)]
struct ExpireHandoffsRequest {
    workspace: String,
    project: String,
    /// Mandatory. Without `confirm: true` the server returns 400.
    confirm: bool,
    /// Only expire handoffs at least this many days old. Omitted clears the
    /// whole open backlog for the scope.
    #[serde(default)]
    older_than_days: Option<u32>,
}

/// `POST /admin/handoffs/expire` — clear the open handoff backlog for one
/// scope.
///
/// Unlike the automatic sweep this does not spare manual or
/// different-directory handoffs. Those exemptions are what the leftover
/// backlog is made of, so honouring them here would expire nothing. It is a
/// state change rather than a delete: the summary and provenance survive and
/// the handoff simply stops being consumable (#513).
async fn handle_expire_handoffs(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    author_ext: Option<axum::Extension<ai_memory_core::UserId>>,
    Json(req): Json<ExpireHandoffsRequest>,
) -> impl IntoResponse {
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "expiring the handoff backlog requires confirm=true"
            })),
        );
    }
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await {
        Ok(ids) => ids,
        Err(e) => return e,
    };

    // Same owner scoping as cancel: a caller clears their own and shared
    // batons, never somebody else's.
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    let owner_filter = ai_memory_core::OwnerFilter::for_actor_context(&actor);
    let author_id = author_ext.map(|axum::Extension(u)| u);

    let older_than_us = req.older_than_days.map(|days| {
        jiff::Timestamp::now().as_microsecond() - i64::from(days) * 24 * 60 * 60 * 1_000_000
    });

    match state
        .writer
        .expire_open_handoffs(ws, proj, owner_filter, older_than_us, author_id)
        .await
    {
        Ok(expired) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "expired": expired,
                "workspace": req.workspace,
                "project": req.project,
            })),
        ),
        Err(e) => internal_err(e.to_string()),
    }
}

async fn handle_pending_writes_list(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<PendingWritesQuery>,
) -> impl IntoResponse {
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let status = match query
        .status
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(raw) => match raw.parse::<AutoImproveProposalStatus>() {
            Ok(status) => Some(status),
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": e.to_string() })),
                );
            }
        },
        None => None,
    };
    match state
        .reader
        .list_auto_improve_proposals(ws, proj, status, query.limit.clamp(1, 200))
        .await
    {
        Ok(list) => (
            StatusCode::OK,
            Json(serde_json::to_value(list).unwrap_or_else(|_| serde_json::json!([]))),
        ),
        Err(e) => internal_err(e.to_string()),
    }
}

async fn pending_detail(
    state: &AdminState,
    raw_id: &str,
    query: &PendingWriteScopeQuery,
) -> Result<
    (
        WorkspaceId,
        ProjectId,
        ai_memory_store::OwnedAutoImproveProposalDetail,
    ),
    (StatusCode, Json<serde_json::Value>),
> {
    let id: AutoImproveProposalId = raw_id.parse().map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid proposal id: {e}") })),
        )
    })?;
    let (ws, proj) = lookup_ws_proj_no_create(state, &query.workspace, &query.project).await?;
    let detail = state
        .reader
        .auto_improve_proposal_detail_with_owner(ws, proj, id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "proposal not found in scope" })),
            )
        })?;
    Ok((ws, proj, detail))
}

async fn handle_pending_write_detail(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<PendingWriteScopeQuery>,
) -> impl IntoResponse {
    match pending_detail(&state, &id, &query).await {
        Ok((_, _, detail)) => (
            StatusCode::OK,
            Json(serde_json::to_value(detail).unwrap_or_else(|_| serde_json::json!({}))),
        ),
        Err(e) => e,
    }
}

async fn handle_pending_write_diff(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<PendingWriteScopeQuery>,
) -> impl IntoResponse {
    match pending_detail(&state, &id, &query).await {
        Ok((ws, proj, owned)) => {
            let detail = &owned.detail;
            let before = state
                .reader
                .page_body_by_ids(ws, proj, detail.summary.target_path.as_str())
                .await
                .ok()
                .flatten()
                .map(|p| p.body)
                .unwrap_or_default();
            let diff = format!(
                "--- before/{path}\n+++ after/{path}\n@@\n{before}\n--- proposed ---\n{after}\n",
                path = detail.summary.target_path.as_str(),
                before = before,
                after = detail.body_markdown
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({ "proposal_id": id, "diff": diff })),
            )
        }
        Err(e) => e,
    }
}

async fn handle_pending_write_approve(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    author_ext: Option<axum::Extension<ai_memory_core::UserId>>,
    level_ext: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    headers: HeaderMap,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<PendingWriteScopeQuery>,
) -> impl IntoResponse {
    let proposal_id: AutoImproveProposalId = match id.parse() {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid proposal id: {e}") })),
            );
        }
    };
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let actor = actor_ext
        .map(|axum::Extension(actor)| actor)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    let author_id = author_ext.map(|axum::Extension(author_id)| author_id);
    let skip_webhooks = skip_webhooks_for_admin_request(level_ext, &headers);
    let admission_ctx = Some(AdmissionContext {
        op: AdmissionOp::WritePage,
        skip_webhooks,
        ..AdmissionContext::default()
    });
    match state
        .wiki
        .approve_auto_improve_proposal(ws, proj, proposal_id, actor, author_id, admission_ctx)
        .await
    {
        Ok(ApproveAutoImproveProposalResult::Approved { page_id }) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "approved", "page_id": page_id.to_string() })),
        ),
        Ok(ApproveAutoImproveProposalResult::Conflict) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "status": "conflict" })),
        ),
        Err(e) => internal_err(e.to_string()),
    }
}

async fn handle_pending_write_reject(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    author_ext: Option<axum::Extension<ai_memory_core::UserId>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(query): Query<PendingWriteScopeQuery>,
    Json(body): Json<PendingRejectRequest>,
) -> impl IntoResponse {
    let proposal_id: AutoImproveProposalId = match id.parse() {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid proposal id: {e}") })),
            );
        }
    };
    let (ws, proj) = match lookup_ws_proj_no_create(&state, &query.workspace, &query.project).await
    {
        Ok(ids) => ids,
        Err(e) => return e,
    };
    let actor = actor_ext
        .map(|axum::Extension(actor)| actor)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    match state
        .writer
        .reject_auto_improve_proposal(RejectAutoImproveProposal {
            workspace_id: ws,
            project_id: proj,
            proposal_id,
            reason: body.reason,
            actor,
            author_id: author_ext.map(|axum::Extension(author_id)| author_id),
        })
        .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "rejected" })),
        ),
        Err(e) => internal_err(e.to_string()),
    }
}

// ---------------------------------------------------------------------
// reorg
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/reorg`.
#[derive(Deserialize)]
struct ReorgRequest {
    /// Show what would change without writing.
    #[serde(default)]
    dry_run: bool,
}

/// One entry in the reorg plan (serialised in the response).
#[derive(Debug, Serialize)]
pub struct ReorgPlanEntry {
    /// Session UUID string.
    pub session_id: String,
    /// Working directory the session was started in.
    pub cwd: String,
    /// Basename-derived project name the session will move to.
    pub new_project: String,
}

/// Summary counts returned alongside the plan.
#[derive(Debug, Serialize)]
pub struct ReorgSummaryJson {
    /// Sessions whose `project_id` was changed.
    pub sessions_moved: usize,
    /// Observations updated to match their session's new project.
    pub observations_updated: usize,
    /// `is_latest=1` pages marked `is_latest=0` (mash-up graveyard).
    pub pages_graveyarded: usize,
    /// Number of distinct per-cwd projects referenced in the plan.
    pub distinct_new_projects: usize,
}

/// Full response for `POST /admin/reorg`.
#[derive(Debug, Serialize)]
pub struct ReorgReport {
    /// `true` when `dry_run` was requested.
    pub dry_run: bool,
    /// All sessions that need (or needed) moving, with their target project.
    pub plan: Vec<ReorgPlanEntry>,
    /// Counts after execution (zeros when `dry_run=true`).
    pub summary: ReorgSummaryJson,
}

/// Read every session that has a non-empty `cwd` field, returning
/// `(session_id, project_id, cwd)` triples ordered by `started_at`.
async fn list_sessions_with_cwd(
    reader: &ai_memory_store::ReaderPool,
    workspace_id: WorkspaceId,
) -> Result<Vec<(SessionId, ProjectId, String)>, StoreError> {
    reader
        .with_conn(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, project_id, cwd \
                     FROM sessions \
                     WHERE workspace_id = ?1 AND cwd IS NOT NULL AND cwd != '' \
                     ORDER BY started_at",
            )?;
            let rows = stmt.query_map([workspace_id.as_bytes().as_slice()], |row| {
                let id_bytes: Vec<u8> = row.get(0)?;
                let proj_bytes: Vec<u8> = row.get(1)?;
                let cwd: String = row.get(2)?;
                Ok((id_bytes, proj_bytes, cwd))
            })?;
            let mut out = Vec::new();
            for r in rows {
                let (id_bytes, proj_bytes, cwd) = r?;
                let sid = SessionId::from_slice(&id_bytes).map_err(StoreError::Memory)?;
                let pid = ProjectId::from_slice(&proj_bytes).map_err(StoreError::Memory)?;
                out.push((sid, pid, cwd));
            }
            Ok(out)
        })
        .await
}

/// Build the reorg plan: for each session with a new project basename,
/// resolve-or-create the target project, return the plan entries plus
/// the set of session updates and the distinct-project count.
async fn build_reorg_plan(
    state: &AdminState,
    ws: WorkspaceId,
    sessions: Vec<(SessionId, ProjectId, String)>,
) -> Result<(Vec<ReorgPlanEntry>, Vec<(SessionId, ProjectId)>, usize), StoreError> {
    // Resolve target project per distinct cwd (basename-derived).
    let mut cwd_to_proj: std::collections::HashMap<String, (WorkspaceId, ProjectId, String)> =
        std::collections::HashMap::new();
    for (_, _, cwd) in &sessions {
        if cwd_to_proj.contains_key(cwd.as_str()) {
            continue;
        }
        let project_name = std::path::Path::new(cwd.as_str())
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| "unknown".to_string());
        let proj = state
            .writer
            .get_or_create_project(ws, project_name.clone(), repo_path_from_reorg_cwd(cwd))
            .await?;
        cwd_to_proj.insert(cwd.clone(), (ws, proj, project_name));
    }

    // Build plan — sessions whose project_id already matches are skipped.
    let mut plan_entries: Vec<ReorgPlanEntry> = Vec::new();
    let mut writer_plan: Vec<(SessionId, ProjectId)> = Vec::new();
    for (session_id, old_project_id, cwd) in &sessions {
        let (_, new_project_id, project_name) = &cwd_to_proj[cwd.as_str()];
        if *new_project_id == *old_project_id {
            continue;
        }
        plan_entries.push(ReorgPlanEntry {
            session_id: session_id.to_string(),
            cwd: cwd.clone(),
            new_project: project_name.clone(),
        });
        writer_plan.push((*session_id, *new_project_id));
    }

    let distinct_new_projects: std::collections::HashSet<ProjectId> =
        writer_plan.iter().map(|(_, pid)| *pid).collect();

    Ok((plan_entries, writer_plan, distinct_new_projects.len()))
}

fn repo_path_from_reorg_cwd(cwd: &str) -> Option<String> {
    let path = std::path::Path::new(cwd);
    let repo_root = ai_memory_consolidate::discover_repo_root(path).ok()?;
    cwd_is_repo_root(path, &repo_root).then(|| repo_root.to_string_lossy().into_owned())
}

fn cwd_is_repo_root(cwd: &std::path::Path, repo_root: &std::path::Path) -> bool {
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(cwd), std::fs::canonicalize(repo_root)) {
        return a == b;
    }
    let strip = |p: &std::path::Path| p.to_string_lossy().trim_end_matches('/').to_string();
    strip(cwd) == strip(repo_root)
}

async fn handle_reorg(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<ReorgRequest>,
) -> impl IntoResponse {
    let ws = match state
        .writer
        .get_or_create_workspace(DEFAULT_WORKSPACE_NAME)
        .await
    {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("workspace: {e}") })),
            );
        }
    };

    let sessions = match list_sessions_with_cwd(&state.reader, ws).await {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
    };

    if sessions.is_empty() {
        let report = ReorgReport {
            dry_run: req.dry_run,
            plan: Vec::new(),
            summary: ReorgSummaryJson {
                sessions_moved: 0,
                observations_updated: 0,
                pages_graveyarded: 0,
                distinct_new_projects: 0,
            },
        };
        return (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        );
    }

    let (plan_entries, writer_plan, distinct_count) =
        match build_reorg_plan(&state, ws, sessions).await {
            Ok(t) => t,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("project: {e}") })),
                );
            }
        };

    if req.dry_run || writer_plan.is_empty() {
        let report = ReorgReport {
            dry_run: req.dry_run,
            plan: plan_entries,
            summary: ReorgSummaryJson {
                sessions_moved: 0,
                observations_updated: 0,
                pages_graveyarded: 0,
                distinct_new_projects: distinct_count,
            },
        };
        return (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        );
    }

    let summary = match state.writer.reorg_sessions(ws, writer_plan).await {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
    };

    let report = ReorgReport {
        dry_run: false,
        plan: plan_entries,
        summary: ReorgSummaryJson {
            sessions_moved: summary.sessions_moved,
            observations_updated: summary.observations_updated,
            pages_graveyarded: summary.pages_graveyarded,
            distinct_new_projects: distinct_count,
        },
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    )
}

// ---------------------------------------------------------------------
// lint
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/lint`.
#[derive(Deserialize)]
struct LintRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Don't write the lint report page.
    #[serde(default)]
    dry_run: bool,
    /// Skip the LLM contradiction pass (rule-based findings only).
    /// When absent, defaults to `false` (LLM pass runs if a provider
    /// is configured).
    #[serde(default)]
    no_llm: bool,
}

fn default_workspace() -> String {
    DEFAULT_WORKSPACE_NAME.to_string()
}

fn default_project() -> String {
    DEFAULT_PROJECT_NAME.to_string()
}

async fn handle_lint(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<LintRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;

    run_lint(
        &state.reader,
        &state.wiki,
        state.llm.as_ref(),
        ws,
        proj,
        ai_memory_consolidate::LintOptions {
            dry_run: req.dry_run,
            use_llm: !req.no_llm,
            decay_lambda: state.decay_params.lambda,
        },
    )
    .await
    .map(|report| {
        (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        )
    })
    .map_err(|e| internal_err(e.to_string()))
}

// ---------------------------------------------------------------------
// forget-sweep
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/forget-sweep`.
#[derive(Deserialize)]
struct ForgetSweepRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist).
    #[serde(default = "default_project")]
    project: String,
    /// Report what would be evicted without actually mutating.
    #[serde(default)]
    dry_run: bool,
}

async fn handle_forget_sweep(
    State(state): State<Arc<AdminState>>,
    axum::Extension(tuning): axum::Extension<SweepTuning>,
    Json(req): Json<ForgetSweepRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;

    run_sweep_with_options(
        &state.reader,
        &state.writer,
        Some(&state.wiki),
        ws,
        proj,
        &state.decay_params,
        tuning.breadth_weight,
        tuning.retention,
        req.dry_run,
    )
    .await
    .map(|report| {
        (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        )
    })
    .map_err(|e| internal_err(e.to_string()))
}

// ---------------------------------------------------------------------
// embed
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/embed`.
#[derive(Deserialize)]
struct EmbedRequest {
    /// Workspace name (must already exist).
    #[serde(default = "default_workspace")]
    workspace: String,
    /// Project name (must already exist). Ignored when
    /// [`Self::all_projects`] is true.
    #[serde(default = "default_project")]
    project: String,
    /// When true, regenerates embeddings even for pages that already
    /// have one matching the current (provider, model, dim).
    #[serde(default)]
    reembed: bool,
    /// When true, count pages that would be embedded/skipped without
    /// calling the embedder or writing anything.
    #[serde(default)]
    dry_run: bool,
    /// When true, embed every project in `workspace` instead of a
    /// single `project`. The CLI sets this for `--force` /
    /// `--reembed` without an explicit `--project` so model migrations
    /// reach stale rows in every namespace.
    #[serde(default)]
    all_projects: bool,
}

/// Summary response from `POST /admin/embed`.
#[derive(Debug, Serialize)]
pub struct EmbedReport {
    /// Pages that were actually embedded (zero in dry-run).
    pub embedded: usize,
    /// Pages skipped because a matching embedding already existed.
    pub skipped: usize,
    /// Pages that failed to embed (read error or provider error).
    pub failed: usize,
    /// Pages that would be embedded in a live run (only meaningful
    /// when `dry_run` was requested).
    pub would_embed: usize,
    /// Provider name.
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// Embedding dimensionality.
    pub dim: u32,
}

async fn embed_project_pages(
    state: &AdminState,
    embedder: &Arc<dyn Embedder>,
    ws: WorkspaceId,
    proj: ProjectId,
    reembed: bool,
    dry_run: bool,
) -> Result<EmbedBackfillCounts, (StatusCode, Json<serde_json::Value>)> {
    run_embedding_backfill(
        &state.reader,
        &state.writer,
        &state.wiki,
        embedder,
        ws,
        proj,
        EmbedBackfillOptions { reembed, dry_run },
    )
    .await
    .map_err(|e| internal_err(e.to_string()))
}

async fn handle_embed(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<EmbedRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let embedder = match state.embedder.clone() {
        Some(e) => e,
        None => {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "embedder not configured on server"
                })),
            ));
        }
    };

    let provider = embedder.provider().to_string();
    let model = embedder.model().to_string();
    let dim = embedder.dim();

    let mut totals = EmbedBackfillCounts::default();

    if req.all_projects {
        if let Some(ws) = state
            .reader
            .find_workspace(req.workspace.clone())
            .await
            .map_err(|e| internal_err(e.to_string()))?
        {
            if req.reembed && !req.dry_run {
                let purged = state
                    .writer
                    .delete_stale_page_embeddings(ws, None, provider.clone(), model.clone(), dim)
                    .await
                    .map_err(|e| internal_err(e.to_string()))?;
                info!(purged, provider = %provider, model = %model, "purged stale page_embeddings before workspace re-embed");
            }

            let summaries = state
                .reader
                .list_projects_with_stats()
                .await
                .map_err(|e| internal_err(e.to_string()))?;
            for summary in summaries
                .into_iter()
                .filter(|p| p.workspace_name == req.workspace)
            {
                let Some(proj) = state
                    .reader
                    .find_project(ws, summary.project_name.clone())
                    .await
                    .map_err(|e| internal_err(e.to_string()))?
                else {
                    continue;
                };
                let partial =
                    embed_project_pages(&state, &embedder, ws, proj, req.reembed, req.dry_run)
                        .await?;
                totals.absorb(partial);
            }
        }
    } else {
        let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;
        if req.reembed && !req.dry_run {
            let purged = state
                .writer
                .delete_stale_page_embeddings(ws, Some(proj), provider.clone(), model.clone(), dim)
                .await
                .map_err(|e| internal_err(e.to_string()))?;
            info!(purged, provider = %provider, model = %model, "purged stale page_embeddings before project re-embed");
        }
        totals = embed_project_pages(&state, &embedder, ws, proj, req.reembed, req.dry_run).await?;
    }

    let report = EmbedReport {
        embedded: totals.embedded,
        skipped: totals.skipped,
        failed: totals.failed,
        would_embed: totals.would_embed,
        provider,
        model,
        dim,
    };
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    ))
}

// ---------------------------------------------------------------------
// commit
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/commit`.
#[derive(Deserialize)]
struct CommitRequest {
    /// Commit message.
    message: String,
}

#[derive(Debug, Serialize)]
struct CheckpointResponse {
    /// Full git commit OID.
    oid: String,
    /// First 12 hex characters, useful for CLI display.
    short_oid: String,
    /// Commit author timestamp in seconds since Unix epoch.
    time: i64,
    /// First line of the commit message.
    summary: String,
}

#[derive(Debug, Deserialize)]
struct CheckpointListQuery {
    /// Maximum number of checkpoints to return.
    #[serde(default = "default_checkpoint_limit")]
    limit: usize,
}

fn default_checkpoint_limit() -> usize {
    20
}

fn short_oid(oid: &str) -> String {
    oid.chars().take(12).collect()
}

fn checkpoint_or_500(
    wiki: &Wiki,
    message: impl AsRef<str>,
) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    wiki.commit_all(message.as_ref())
        .map(|oid| oid.map(|oid| oid.to_string()))
        .map_err(|e| internal_err(e.to_string()))
}

fn checkpoint_or_warn(wiki: &Wiki, message: impl AsRef<str>) -> Option<String> {
    match wiki.commit_all(message.as_ref()) {
        Ok(Some(oid)) => Some(oid.to_string()),
        Ok(None) => None,
        Err(e) => {
            warn!(error = %e, "wiki checkpoint failed after mutation");
            None
        }
    }
}

async fn handle_commit(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<CommitRequest>,
) -> impl IntoResponse {
    match state.wiki.commit_all(&req.message) {
        Ok(Some(oid)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "committed": true,
                "oid": oid.to_string(),
            })),
        ),
        Ok(None) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "committed": false,
                "reason": "nothing to commit",
            })),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

async fn handle_checkpoints(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<CheckpointListQuery>,
) -> impl IntoResponse {
    let limit = query.limit.clamp(1, 100);
    match state.wiki.recent_checkpoints(limit) {
        Ok(checkpoints) => {
            let checkpoints: Vec<CheckpointResponse> = checkpoints
                .into_iter()
                .map(|cp| CheckpointResponse {
                    short_oid: short_oid(&cp.oid),
                    oid: cp.oid,
                    time: cp.time,
                    summary: cp.summary,
                })
                .collect();
            (
                StatusCode::OK,
                Json(serde_json::to_value(checkpoints).unwrap_or_else(|_| serde_json::json!([]))),
            )
        }
        Err(e) => internal_err(e.to_string()),
    }
}

// ---------------------------------------------------------------------
// restore-page
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct RestorePageRequest {
    /// Workspace name. Must exist.
    workspace: String,
    /// Project name. Must exist within `workspace`.
    project: String,
    /// Relative wiki path to restore.
    path: String,
    /// Git revision/commit to restore from.
    rev: String,
}

#[derive(Serialize)]
struct RestorePageResponse {
    /// UUID of the restored page version.
    page_id: String,
    /// Canonical wiki path restored.
    path: String,
    /// Revision requested by the caller.
    restored_from: String,
    /// Pre-restore checkpoint, if the current tree had changes to save.
    #[serde(skip_serializing_if = "Option::is_none")]
    pre_checkpoint: Option<String>,
    /// Post-restore checkpoint, if the restore changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
}

async fn handle_restore_page(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<RestorePageRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let path = PagePath::new(req.path.clone()).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": format!("invalid path: {e}") })),
        )
    })?;
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;

    let pre_checkpoint = checkpoint_or_500(
        &state.wiki,
        format!(
            "pre-restore-page {}/{}: {}",
            req.workspace,
            req.project,
            path.as_str()
        ),
    )?;

    let page_id = state
        .wiki
        .restore_page_from_checkpoint(ws, proj, path.clone(), &req.rev)
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!(
            "restore-page {}/{}: {} from {}",
            req.workspace,
            req.project,
            path.as_str(),
            req.rev
        ),
    );

    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(RestorePageResponse {
                page_id: page_id.to_string(),
                path: path.to_string(),
                restored_from: req.rev,
                pre_checkpoint,
                checkpoint,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
    ))
}

// ---------------------------------------------------------------------
// purge-session
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/purge-session`.
#[derive(Deserialize)]
struct PurgeSessionRequest {
    /// Workspace name. Must already exist; 404 otherwise.
    workspace: String,
    /// Project name. Must already exist; 404 otherwise.
    project: String,
    /// Full `sessions.id` UUID. A session that does not belong to the named
    /// workspace/project is a 404 — the id alone is never authority over
    /// another scope.
    session_id: String,
    /// Mandatory confirmation flag. Without `confirm: true` the server
    /// returns 400 — purging is destructive and irreversible.
    confirm: bool,
    /// Reclaim freed bytes: rebuild the FTS indexes and `VACUUM` after the
    /// delete commits. Off by default; see `ai_memory_store::Compaction` for
    /// the cost and for what it does not guarantee.
    #[serde(default)]
    compact: bool,
}

/// `POST /admin/purge-session` — delete one session and everything derived
/// from it, inside a single workspace/project scope.
async fn handle_purge_session(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    author_ext: Option<axum::Extension<ai_memory_core::UserId>>,
    Json(req): Json<PurgeSessionRequest>,
) -> impl IntoResponse {
    let author_id = author_ext.map(|axum::Extension(u)| u);
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "destructive operation requires confirm=true"
            })),
        );
    }

    let session_id = match req.session_id.trim().parse::<SessionId>() {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "session_id must be a full UUID" })),
            );
        }
    };

    let (ws_id, proj_id) =
        match lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await {
            Ok(ids) => ids,
            Err(e) => return e,
        };

    // Admission runs before anything is deleted so a reject-policy webhook can
    // refuse while every row is still intact — same order as purge-project.
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    let ctx = AdmissionContext {
        workspace: req.workspace.clone(),
        project: req.project.clone(),
        op: AdmissionOp::PurgeSession,
        actor,
        ..Default::default()
    };
    if let Err(e) = state
        .wiki
        .admit_purge_session(ws_id, proj_id, Some(ctx))
        .await
    {
        return internal_err(e.to_string());
    }

    let compaction = if req.compact {
        ai_memory_store::Compaction::Reclaim
    } else {
        ai_memory_store::Compaction::Skip
    };

    let summary = match state
        .writer
        .purge_session(ws_id, proj_id, session_id, author_id, compaction)
        .await
    {
        Ok(s) => s,
        // Absent from this scope (or already purged) is a 404, not a fault.
        Err(e @ StoreError::NotFound(_)) => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
        Err(e) => return internal_err(e.to_string()),
    };

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "session_id": session_id.to_string(),
            "workspace": req.workspace,
            "project": req.project,
            "observations_deleted": summary.observations_deleted,
            "handoffs_deleted": summary.handoffs_deleted,
            "pages_deleted": summary.pages_deleted,
            "auto_improve_runs_deleted": summary.auto_improve_runs_deleted,
            "removed_paths": summary.removed_paths,
            "compacted": summary.compacted,
        })),
    )
}

// ---------------------------------------------------------------------
// purge-project
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/purge-project`.
#[derive(Deserialize)]
struct PurgeProjectRequest {
    /// Workspace name. The workspace must already exist; we 404 if absent.
    workspace: String,
    /// Project name. The project must already exist; we 404 if absent.
    project: String,
    /// Mandatory confirmation flag. Without `confirm: true` the server
    /// returns 400 — purging is destructive and irreversible.
    confirm: bool,
    /// Purge even when a managed workstream under this project still holds a
    /// live run lease. Off by default: the cascade would delete that lease row
    /// out from under a running agent, which then cannot save its history.
    #[serde(default)]
    force: bool,
    /// Reclaim freed bytes: rebuild the FTS indexes and `VACUUM` after the
    /// delete commits. Off by default; see `ai_memory_store::Compaction` for
    /// the cost and for what it does not guarantee.
    #[serde(default)]
    compact: bool,
}

/// Wire-format summary returned by `POST /admin/purge-project`.
#[derive(Debug, Serialize)]
pub struct PurgeProjectReport {
    /// Human-readable `workspace/project` label.
    pub label: String,
    /// Number of `pages` rows deleted (all versions).
    pub pages_deleted: u64,
    /// Number of `sessions` rows deleted.
    pub sessions_deleted: u64,
    /// Number of `observations` rows deleted.
    pub observations_deleted: u64,
    /// Number of `handoffs` rows deleted.
    pub handoffs_deleted: u64,
    /// Number of `page_embeddings` rows deleted.
    pub embeddings_deleted: u64,
    /// Number of managed `workstreams` rows deleted via cascade.
    pub workstreams_deleted: u64,
    /// Number of `managed_runs` rows deleted via cascade.
    pub managed_runs_deleted: u64,
    /// Ids of the deleted workstreams whose `raw/workstreams/<id>/` segment
    /// directories were included in the post-commit cleanup.
    pub workstream_ids: Vec<String>,
    /// Paths removed from disk (the project's UUID-namespaced directory).
    pub files_deleted: Vec<String>,
    /// Paths that could not be removed from disk (non-fatal; DB rows are gone).
    pub files_failed: Vec<String>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran). False for a
    /// plain logical delete: the rows are gone from the API and from search,
    /// but their bytes stay in free pages until the file is next rewritten.
    pub compacted: bool,
    /// Pre-purge checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-purge checkpoint, if the purge changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

async fn remove_workstream_segment_storage(
    state: &AdminState,
    workstream_ids: &[String],
    operation: &'static str,
) -> (Vec<String>, Vec<String>) {
    let mut deleted = Vec::with_capacity(workstream_ids.len());
    let mut failed = Vec::new();

    for workstream_id in workstream_ids {
        let path = state
            .data_dir
            .join("raw")
            .join("workstreams")
            .join(workstream_id);
        let path_display = path.display().to_string();
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => deleted.push(path_display),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                warn!(
                    operation,
                    path = %path_display,
                    error = %error,
                    "scope purge failed to remove workstream segment directory"
                );
                failed.push(path_display);
            }
        }
    }

    (deleted, failed)
}

async fn remove_purged_project_storage(
    state: &AdminState,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    workstream_ids: &[String],
    operation: &'static str,
) -> (Vec<String>, Vec<String>) {
    let project_root = state.wiki.project_root(workspace_id, project_id);
    let project_root_display = project_root.display().to_string();
    let mut deleted = Vec::with_capacity(workstream_ids.len() + 1);
    let mut failed = Vec::new();

    match state
        .wiki
        .remove_project_dir(workspace_id, project_id)
        .await
    {
        Ok(()) => deleted.push(project_root_display.clone()),
        Err(error) => {
            warn!(
                operation,
                path = %project_root_display,
                error = %error,
                "project purge failed to remove wiki directory"
            );
            failed.push(project_root_display);
        }
    }

    let (raw_deleted, raw_failed) =
        remove_workstream_segment_storage(state, workstream_ids, operation).await;
    deleted.extend(raw_deleted);
    failed.extend(raw_failed);

    (deleted, failed)
}

async fn handle_purge_project(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    author_ext: Option<axum::Extension<ai_memory_core::UserId>>,
    Json(req): Json<PurgeProjectRequest>,
) -> impl IntoResponse {
    let author_id = author_ext.map(|axum::Extension(u)| u);
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "destructive operation requires confirm=true"
            })),
        );
    }

    // Look up workspace and project IDs without auto-creating.
    let (ws_id, proj_id) =
        match lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await {
            Ok(ids) => ids,
            Err(e) => return e,
        };

    let label = format!("{}/{}", req.workspace, req.project);

    // Admission must run before any destructive work. A reject-policy webhook
    // is allowed to abort the purge while DB rows and files are still intact.
    // Seed names from the request so mirrors do not depend on DB lookup after
    // the rows are purged below. Carry the authenticated actor so a scope-guard
    // admission webhook can authorize the purge by user — an empty actor is
    // rejected (`403 user '' not allowed to purge_project`).
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    let purge_ctx = AdmissionContext {
        workspace: req.workspace.clone(),
        project: req.project.clone(),
        op: AdmissionOp::PurgeProject,
        actor,
        ..Default::default()
    };
    let resolved_purge_ctx = match state
        .wiki
        .admit_purge_project(ws_id, proj_id, Some(purge_ctx))
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => return internal_err(e.to_string()),
    };

    let pre_checkpoint = match checkpoint_or_500(&state.wiki, format!("pre-purge-project {label}"))
    {
        Ok(oid) => oid,
        Err(e) => return e,
    };

    let compaction = if req.compact {
        ai_memory_store::Compaction::Reclaim
    } else {
        ai_memory_store::Compaction::Skip
    };

    let summary = match state
        .writer
        .purge_project(ws_id, proj_id, &label, author_id, req.force, compaction)
        .await
    {
        Ok(s) => s,
        // A live managed run is a conflict, not a server fault: the operator
        // can finish the session or retry with `force`. Same status the
        // heartbeat itself returns when a lease is gone, so the two agree.
        Err(e @ StoreError::ManagedRunActive { .. }) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
        Err(e) => return internal_err(e.to_string()),
    };

    // DB cascade already deleted all rows. Remove both the wiki project root
    // and every raw managed-workstream segment directory best-effort, reporting
    // partial failures without disguising the committed database purge.
    let (files_deleted, files_failed) = remove_purged_project_storage(
        &state,
        ws_id,
        proj_id,
        &summary.workstream_ids,
        "purge-project",
    )
    .await;
    // Mirrors that track filesystem reality (a git-push mirror) want to
    // know the on-disk dir is still present even though the DB rows are
    // gone, so they can refuse to drop their own copy in violation of
    // their source of truth. Mirrors that track DB intent can ignore
    // `partial_failure`. Default-skipped on the wire so existing
    // webhook consumers see no extra key.
    let mut dispatch_ctx = resolved_purge_ctx;
    if !files_failed.is_empty()
        && let Some(ref mut c) = dispatch_ctx
    {
        c.partial_failure = true;
    }
    state.wiki.dispatch_purge_project(dispatch_ctx.as_ref());

    state.active_project.clear_project(proj_id);
    invalidate_scope_cache(&state, ScopeInvalidation::Project(proj_id)).await;

    let checkpoint = checkpoint_or_warn(&state.wiki, format!("purge-project {label}"));

    let report = PurgeProjectReport {
        label: summary.label,
        pages_deleted: summary.pages_deleted,
        sessions_deleted: summary.sessions_deleted,
        observations_deleted: summary.observations_deleted,
        handoffs_deleted: summary.handoffs_deleted,
        embeddings_deleted: summary.embeddings_deleted,
        workstreams_deleted: summary.workstreams_deleted,
        managed_runs_deleted: summary.managed_runs_deleted,
        workstream_ids: summary.workstream_ids,
        compacted: summary.compacted,
        files_deleted,
        files_failed,
        pre_checkpoint,
        checkpoint,
    };

    (
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
    )
}

// ---------------------------------------------------------------------
// rename-project
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/rename-workspace`.
#[derive(Deserialize)]
struct RenameWorkspaceRequest {
    /// Current workspace name (must exist).
    from: String,
    /// New workspace name. Must be non-empty, no slashes, and not already used.
    to: String,
}

/// Wire-format summary returned by `POST /admin/rename-workspace`.
#[derive(Debug, Serialize)]
pub struct RenameWorkspaceResult {
    /// Previous workspace name.
    pub from: String,
    /// New workspace name.
    pub to: String,
    /// Post-rename mirror checkpoint, if one was taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// Number of scope manifests refreshed after the SQLite rename.
    pub manifests_refreshed: usize,
    /// Non-fatal manifest refresh failure after the SQLite rename committed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_warning: Option<String>,
}

/// `POST /admin/rename-workspace` — rename a workspace (column-only; the
/// on-disk dir is UUID-keyed, so nothing moves). 404 unknown, 422 taken/invalid.
async fn handle_rename_workspace(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<RenameWorkspaceRequest>,
) -> impl IntoResponse {
    let ws_id = match lookup_ws_no_create(&state, &req.from).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    if let Err(e) = state.writer.rename_workspace(ws_id, req.to.clone()).await {
        let status = match &e {
            StoreError::WorkspaceNameTaken(_) | StoreError::InvalidWorkspaceName(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            StoreError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        return (status, Json(serde_json::json!({ "error": e.to_string() })));
    }
    let (manifests_refreshed, manifest_warning) = match state.wiki.backfill_scope_manifests().await
    {
        Ok(n) => (n, None),
        Err(e) => {
            warn!(error = %e, "rename-workspace: scope-manifest backfill failed after rename");
            (0, Some(e.to_string()))
        }
    };
    invalidate_scope_cache(&state, ScopeInvalidation::Workspace(ws_id)).await;
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!("rename-workspace {} -> {}", req.from, req.to),
    );
    let result = RenameWorkspaceResult {
        from: req.from.clone(),
        to: req.to.clone(),
        checkpoint,
        manifests_refreshed,
        manifest_warning,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&result).unwrap_or(serde_json::Value::Null)),
    )
}

/// JSON request body for `POST /admin/delete-workspace`.
#[derive(Deserialize)]
struct DeleteWorkspaceRequest {
    /// Workspace name to delete (must exist).
    workspace: String,
    /// Delete even when the workspace still holds projects. Without it, a
    /// non-empty workspace is refused so a typo can't wipe live data.
    #[serde(default)]
    force: bool,
    /// Reclaim freed bytes: rebuild the FTS indexes and `VACUUM` after the
    /// delete commits. Off by default; see `ai_memory_store::Compaction` for
    /// the cost and for what it does not guarantee.
    #[serde(default)]
    compact: bool,
}

/// Wire-format summary returned by `POST /admin/delete-workspace`.
#[derive(Debug, Serialize)]
pub struct DeleteWorkspaceResult {
    /// Workspace name that was deleted.
    pub workspace: String,
    /// Projects removed via the `workspace_id` cascade.
    pub projects_deleted: u64,
    /// `pages` rows removed via cascade (all versions).
    pub pages_deleted: u64,
    /// Managed `workstreams` rows removed via cascade.
    pub workstreams_deleted: u64,
    /// `managed_runs` rows removed via cascade.
    pub managed_runs_deleted: u64,
    /// Ids of the deleted workstreams whose raw segment directories were
    /// included in the post-commit cleanup.
    pub workstream_ids: Vec<String>,
    /// Paths removed from disk (the workspace directory and raw segments).
    pub files_deleted: Vec<String>,
    /// Paths that could not be removed from disk (non-fatal; DB rows are gone).
    pub files_failed: Vec<String>,
    /// Whether the freed bytes were reclaimed (`VACUUM` ran). False for a
    /// plain logical delete: the rows are gone from the API and from search,
    /// but their bytes stay in free pages until the file is next rewritten.
    pub compacted: bool,
    /// Pre-delete checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-delete mirror checkpoint, if one was taken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// `POST /admin/delete-workspace` — remove a workspace and, via cascade, every
/// project/page under it. Guarded: refuses a non-empty workspace unless
/// `force` (a typo shouldn't wipe live data). Orphan-workspace cleanup.
/// JSON request body for `POST /admin/compact`.
#[derive(Deserialize)]
struct CompactRequest {
    /// Mandatory confirmation. Not because compaction destroys anything — it
    /// deletes nothing — but because it takes an exclusive lock and rewrites
    /// the whole database, so every write blocks until it finishes. That is an
    /// availability decision, and it should be deliberate.
    confirm: bool,
}

/// Wire-format summary returned by `POST /admin/compact`.
#[derive(Debug, Serialize)]
pub struct CompactReport {
    /// Database size in bytes before the rebuild + `VACUUM`.
    pub bytes_before: u64,
    /// Database size in bytes afterwards.
    pub bytes_after: u64,
    /// Bytes returned to the filesystem (saturating at zero).
    pub bytes_reclaimed: u64,
}

/// `POST /admin/compact` — rebuild the FTS indexes and `VACUUM`, deleting
/// nothing. The on-demand form of what the destructive commands offer as
/// `--compact`.
async fn handle_compact(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<CompactRequest>,
) -> impl IntoResponse {
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "compact rewrites the whole database under an exclusive \
                          lock; pass confirm: true to proceed"
            })),
        );
    }
    match state.writer.compact().await {
        Ok(summary) => {
            let report = CompactReport {
                bytes_before: summary.bytes_before,
                bytes_after: summary.bytes_after,
                bytes_reclaimed: summary.bytes_reclaimed(),
            };
            (
                StatusCode::OK,
                Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
    }
}

async fn handle_delete_workspace(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Json(req): Json<DeleteWorkspaceRequest>,
) -> impl IntoResponse {
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    let compaction = if req.compact {
        ai_memory_store::Compaction::Reclaim
    } else {
        ai_memory_store::Compaction::Skip
    };
    match delete_workspace_core(&state, &req.workspace, req.force, actor, compaction).await {
        Ok(result) => (
            StatusCode::OK,
            Json(serde_json::to_value(&result).unwrap_or(serde_json::Value::Null)),
        ),
        Err(resp) => resp,
    }
}

/// Delete a workspace and, via cascade, every project/page under it — returning
/// the summary as data (or the HTTP error a handler surfaces verbatim). Guarded:
/// refuses a non-empty workspace unless `force`. Shared by
/// `handle_delete_workspace` and `handle_merge_workspace` (which drains the
/// source workspace first, then deletes the now-empty shell with `force=false`).
async fn delete_workspace_core(
    state: &Arc<AdminState>,
    workspace: &str,
    force: bool,
    actor: ai_memory_core::ActorContext,
    compaction: ai_memory_store::Compaction,
) -> Result<DeleteWorkspaceResult, MoveErr> {
    let ws_id = lookup_ws_no_create(state, workspace).await?;

    if !force {
        match state
            .reader
            .list_projects_with_stats_for_workspace(workspace.to_string())
            .await
        {
            Ok(projects) if !projects.is_empty() => {
                return Err((
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": StoreError::WorkspaceNotEmpty(projects.len() as u64).to_string()
                    })),
                ));
            }
            Ok(_) => {}
            Err(e) => return Err(internal_err(e.to_string())),
        }
    }

    let purge_ctx = AdmissionContext {
        workspace: workspace.to_string(),
        project: String::new(),
        op: AdmissionOp::PurgeWorkspace,
        actor,
        ..Default::default()
    };
    let resolved_purge_ctx = match state
        .wiki
        .admit_purge_workspace(ws_id, Some(purge_ctx))
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => return Err(internal_err(e.to_string())),
    };

    let pre_checkpoint =
        checkpoint_or_500(&state.wiki, format!("pre-delete-workspace {workspace}"))?;

    let summary = match state
        .writer
        .delete_workspace(ws_id, force, compaction)
        .await
    {
        Ok(s) => s,
        Err(e) => {
            let status = match &e {
                StoreError::WorkspaceNotEmpty(_) => StatusCode::CONFLICT,
                StoreError::NotFound(_) => StatusCode::NOT_FOUND,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            };
            return Err((status, Json(serde_json::json!({ "error": e.to_string() }))));
        }
    };

    let ws_root_str = state
        .wiki
        .root()
        .join(ws_id.to_string())
        .display()
        .to_string();
    let mut files_deleted = Vec::with_capacity(summary.workstream_ids.len() + 1);
    let mut files_failed = Vec::new();
    match state.wiki.remove_workspace_dir(ws_id).await {
        Ok(()) => files_deleted.push(ws_root_str.clone()),
        Err(e) => {
            warn!(error = %e, workspace = %workspace, path = %ws_root_str, "delete-workspace: on-disk dir removal failed");
            files_failed.push(ws_root_str);
        }
    }
    let (raw_deleted, raw_failed) =
        remove_workstream_segment_storage(state, &summary.workstream_ids, "delete-workspace").await;
    files_deleted.extend(raw_deleted);
    files_failed.extend(raw_failed);

    let mut dispatch_ctx = resolved_purge_ctx;
    if !files_failed.is_empty()
        && let Some(ref mut c) = dispatch_ctx
    {
        c.partial_failure = true;
    }
    state.wiki.dispatch_purge_workspace(dispatch_ctx.as_ref());

    state.active_project.clear_workspace(ws_id);
    invalidate_scope_cache(state, ScopeInvalidation::Workspace(ws_id)).await;

    Ok(DeleteWorkspaceResult {
        workspace: workspace.to_string(),
        projects_deleted: summary.projects_deleted,
        pages_deleted: summary.pages_deleted,
        workstreams_deleted: summary.workstreams_deleted,
        managed_runs_deleted: summary.managed_runs_deleted,
        workstream_ids: summary.workstream_ids,
        compacted: summary.compacted,
        files_deleted,
        files_failed,
        pre_checkpoint,
        checkpoint: checkpoint_or_warn(&state.wiki, format!("delete-workspace {workspace}")),
    })
}

/// JSON request body for `POST /admin/rename-project`.
#[derive(Deserialize)]
struct RenameProjectRequest {
    /// Workspace name (must exist; we don't auto-create on rename).
    workspace: String,
    /// Current project name.
    from: String,
    /// New project name. Must be non-empty, no slashes.
    to: String,
}

/// Wire-format summary returned by `POST /admin/rename-project`.
#[derive(Debug, Serialize)]
pub struct RenameProjectSummary {
    /// Workspace name.
    pub workspace: String,
    /// Previous project name.
    pub from: String,
    /// New project name.
    pub to: String,
    /// Number of `is_latest=1` pages now under the renamed project.
    /// No files move — this is purely an informational count.
    pub pages: u64,
    /// Post-rename checkpoint, if `_meta.md` changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

async fn handle_rename_project(
    State(state): State<Arc<AdminState>>,
    author_ext: Option<axum::Extension<ai_memory_core::UserId>>,
    Json(req): Json<RenameProjectRequest>,
) -> impl IntoResponse {
    let author_id = author_ext.map(|axum::Extension(u)| u);
    // Look up workspace + source project; 404 if either is absent.
    let (ws_id, proj_id) = match lookup_ws_proj_no_create(&state, &req.workspace, &req.from).await {
        Ok(ids) => ids,
        Err(e) => return e,
    };

    // Step 3: execute the rename. The writer validates the name and
    // returns ProjectNameTaken / InvalidProjectName on conflicts. A
    // `NotFound` here signals a race — typically a concurrent
    // `purge-project` deleted the row between the lookup above and
    // this UPDATE. Map it to 404 so the caller sees an honest failure
    // instead of the previous false-200.
    if let Err(e) = state
        .writer
        .rename_project(ws_id, proj_id, req.to.clone(), author_id)
        .await
    {
        let status = match &e {
            StoreError::ProjectNameTaken(_) | StoreError::InvalidProjectName(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            StoreError::NotFound(_) => StatusCode::NOT_FOUND,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        return (status, Json(serde_json::json!({ "error": e.to_string() })));
    }

    // Step 4: count is_latest pages that now belong to the renamed project.
    // COUNT(*) always produces a row, so no optional() needed. We pass the
    // 16-byte project id as a plain &[u8] slice to avoid importing rusqlite
    // (not a direct dependency of this crate).
    let pid_bytes = *proj_id.as_bytes();
    let pages = match state
        .reader
        .with_conn(move |conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM pages WHERE project_id = ?1 AND is_latest = 1",
                [&pid_bytes[..]],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(n).unwrap_or(0))
        })
        .await
    {
        Ok(n) => n,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            );
        }
    };

    let summary = RenameProjectSummary {
        workspace: req.workspace.clone(),
        from: req.from.clone(),
        to: req.to.clone(),
        pages,
        checkpoint: {
            if let Err(e) = state.wiki.backfill_scope_manifests().await {
                warn!(error = %e, "rename-project: scope-manifest backfill failed after rename");
            }
            checkpoint_or_warn(
                &state.wiki,
                format!(
                    "rename-project {}/{} -> {}",
                    req.workspace, req.from, req.to
                ),
            )
        },
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&summary).unwrap_or_else(|_| serde_json::json!({}))),
    )
}

// ---------------------------------------------------------------------
// move-project
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/move-project`.
#[derive(Deserialize)]
struct MoveProjectRequest {
    /// Source workspace. Must already exist; we 404 if absent.
    from_workspace: String,
    /// Project name to move. Must already exist in `from_workspace`; 404 if absent.
    project: String,
    /// Destination workspace. Auto-created if absent.
    to_workspace: String,
    /// Mandatory confirmation flag. The move PURGES the source after
    /// copying, so without `confirm: true` the server returns 400.
    confirm: bool,
    /// Override the active-project guard. By default the server refuses (409)
    /// to move the project the hook router is currently writing to, since a
    /// live session's next observation would carry a stale workspace id.
    /// `force: true` proceeds anyway (still safe: the move republishes the
    /// active pointer and the (workspace_id, project_id) trigger makes any
    /// stale write fail cleanly rather than corrupt). It never overrides a
    /// live managed-workstream lease in the destructive copy-purge path.
    #[serde(default)]
    force: bool,
    /// Policy for the copy-purge MERGE path when a source page's path already
    /// exists in the destination with DIFFERENT content. Ignored by true-move
    /// (no copy). Default `block` — the safe choice for a destructive op.
    #[serde(default)]
    on_conflict: OnConflict,
}

/// What to do when a merged page path collides with an existing destination
/// page of different content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum OnConflict {
    /// Abort the whole move (source untouched), listing the conflicting paths.
    /// The operator resolves them or picks another policy explicitly.
    #[default]
    Block,
    /// The source page supersedes the destination page at the same path (the
    /// destination's prior version becomes history).
    Overwrite,
    /// Keep BOTH: the source page lands under a de-duplicated path
    /// (`<stem>-from-<src_workspace>.md`), reported in `conflicts`.
    Duplicate,
}

/// Wire-format report returned by `POST /admin/move-project`.
#[derive(Debug, Serialize)]
pub struct MoveProjectReport {
    /// `from_workspace/project` label.
    pub from: String,
    /// `to_workspace/project` label.
    pub to: String,
    /// `true` when the destination workspace already held a same-named
    /// project — the copy MERGED into it rather than creating a fresh one.
    pub merged_into_existing: bool,
    /// How the move was performed:
    /// - `"true-move"`: a lossless cross-workspace re-stamp (same `project_id`,
    ///   one SQL transaction + one dir rename). Used when the destination has
    ///   no same-named project. Sessions/observations/handoffs and the full
    ///   supersession history all survive; nothing is re-embedded.
    /// - `"copy-purge"`: the destination already held a same-named project, so
    ///   the source's latest pages were copied in and merged, then the source
    ///   purged. Only durable pages move; episodic rows are dropped.
    pub moved_via: &'static str,
    /// Number of latest pages copied into the destination (copy-purge) or
    /// re-stamped in place (true-move).
    pub pages_copied: u64,
    /// Managed workstreams re-stamped in place by a lossless true move.
    /// Copy-purge reports zero because it does not transfer managed history.
    pub workstreams_moved: u64,
    /// Source paths whose on-disk file could not be read (copy skipped).
    /// When non-empty the source is NOT purged so a fixed re-run is safe.
    pub pages_skipped: Vec<String>,
    /// Whether the source project was purged (only when every page copied).
    pub source_purged: bool,
    /// Source `pages` rows deleted by the purge (all versions).
    pub source_pages_deleted: u64,
    /// Source `sessions` rows deleted by the purge.
    pub source_sessions_deleted: u64,
    /// Source `observations` rows deleted by the purge.
    pub source_observations_deleted: u64,
    /// Source `handoffs` rows deleted by the purge.
    pub source_handoffs_deleted: u64,
    /// Source `page_embeddings` rows deleted by the purge.
    pub source_embeddings_deleted: u64,
    /// Source on-disk dirs removed.
    pub files_deleted: Vec<String>,
    /// Source on-disk dirs that could not be removed (non-fatal).
    pub files_failed: Vec<String>,
    /// Same-path conflicts in the copy-purge merge: a source page whose path
    /// already existed in the destination (with different content) was landed
    /// under a de-duplicated path so BOTH survive. Each entry is the original
    /// source path and the de-duplicated destination path it was written to.
    pub conflicts: Vec<PathConflict>,
    /// Pre-move checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-move checkpoint, if the move changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// One same-path collision resolved by de-duplicating the source page's path.
#[derive(Debug, Serialize)]
pub struct PathConflict {
    /// The source page's original path (also the destination's existing path).
    pub path: String,
    /// The de-duplicated path the source page was written to instead.
    pub moved_to: String,
}

/// An error response from a move: the HTTP status + JSON body a handler would
/// have returned directly. The move core returns this as the `Err` arm so a
/// single move can be surfaced verbatim by `handle_move_project` or aggregated
/// by `handle_merge_workspace`.
type MoveErr = (StatusCode, Json<serde_json::Value>);

/// Lossless cross-workspace move: under the wiki mutation gate, rename the
/// project's on-disk dir to the destination workspace, then re-stamp its
/// `workspace_id` across every domain table (one transaction, same
/// `project_id`). The caller has already verified the destination has no
/// same-named project.
async fn true_move_project(
    state: &Arc<AdminState>,
    req: &MoveProjectRequest,
    src_ws: WorkspaceId,
    src_proj: ProjectId,
    pre_checkpoint: Option<String>,
    actor: ai_memory_core::ActorContext,
) -> Result<MoveProjectReport, MoveErr> {
    // Ensure the destination workspace ROW exists (FK target for the
    // re-stamp) without creating a new project — the existing project_id is
    // what we move.
    let dst_ws = match state
        .writer
        .get_or_create_workspace(req.to_workspace.clone())
        .await
    {
        Ok(ws) => ws,
        Err(e) => return Err(internal_err(e.to_string())),
    };

    // A true move targets a FRESH destination, so its dir must not already
    // exist. Wiki::move_project_workspace repeats this check under the
    // exclusive mutation guard before it renames anything.
    let dst_dir = state.wiki.project_root(dst_ws, src_proj);
    if dst_dir.exists() {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "destination dir already exists: {}; refusing true-move",
                    dst_dir.display()
                )
            })),
        ));
    }

    let move_ctx = AdmissionContext {
        workspace: req.from_workspace.clone(),
        project: req.project.clone(),
        destination_workspace: Some(req.to_workspace.clone()),
        destination_project: Some(req.project.clone()),
        op: AdmissionOp::MoveProject,
        actor,
        ..Default::default()
    };

    // Wiki owns the critical section: it runs move admission, renames the dir,
    // re-stamps SQLite, and rolls the dir back on SQL failure while normal page
    // writes/reindexes are blocked by the same process-local gate.
    let summary = match state
        .wiki
        .move_project_workspace(src_proj, src_ws, dst_ws, Some(move_ctx))
        .await
    {
        Ok(s) => s,
        Err(WikiError::Store(StoreError::NotFound(msg))) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": msg })),
            ));
        }
        Err(e @ WikiError::DestinationExists(_)) => {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({ "error": e.to_string() })),
            ));
        }
        Err(e) => {
            warn!(error = %e, "true-move: move aborted before completion");
            return Err(internal_err(format!("move aborted (nothing changed): {e}")));
        }
    };

    // Keep the in-process active-project pointer correct: the project_id is
    // unchanged, only its workspace moved. If a hook had published this project
    // as active, republish it under the destination workspace so the next event
    // resolves cleanly (rather than tripping the pairing trigger first).
    state
        .active_project
        .retarget_project_workspace(src_proj, dst_ws);
    // Proactively drop any stale per-cwd cache entry for the moved project.
    invalidate_scope_cache(state, ScopeInvalidation::Project(src_proj)).await;

    if let Err(e) = state.wiki.backfill_scope_manifests().await {
        warn!(error = %e, "true-move: scope-manifest backfill failed after move");
    }
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!(
            "move-project {}/{} -> {}/{}",
            req.from_workspace, req.project, req.to_workspace, req.project
        ),
    );

    let report = MoveProjectReport {
        from: format!("{}/{}", req.from_workspace, req.project),
        to: format!("{}/{}", req.to_workspace, req.project),
        merged_into_existing: false,
        moved_via: "true-move",
        pages_copied: summary.pages_moved,
        workstreams_moved: summary.workstreams_moved,
        pages_skipped: Vec::new(),
        // Nothing is purged in a true move — the source rows ARE the
        // destination rows, just re-stamped.
        source_purged: false,
        source_pages_deleted: 0,
        source_sessions_deleted: 0,
        source_observations_deleted: 0,
        source_handoffs_deleted: 0,
        source_embeddings_deleted: 0,
        files_deleted: Vec::new(),
        files_failed: Vec::new(),
        // A true move never copies pages, so it never has a path conflict.
        conflicts: Vec::new(),
        pre_checkpoint,
        checkpoint,
    };
    Ok(report)
}

/// Token inserted between the original stem and the source-workspace
/// slug when copy-purge merges conflicting page paths
/// (`<stem>-from-<src_workspace>.md`). The literal also appears in
/// `docs/lifecycle-ops.md`; keep these two in sync.
const DEDUP_FROM_TOKEN: &str = "-from-";

/// Char allowlist for the source-workspace slug embedded in a deduped
/// destination path. ASCII alphanumeric plus `-` / `_` keeps the slug
/// safe inside a filesystem path component on every supported platform
/// (Windows treats `:`, `*`, `?`, `<`, `>`, `|` as illegal; Linux
/// tolerates more but UTF-8 mojibake in filenames is a maintenance
/// hazard). Everything else collapses to a single `-` separator,
/// preserving readability without introducing path-traversal vectors.
fn is_dedup_slug_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Pick a destination path that collides with neither an existing destination
/// page nor one already claimed in this run. Keeps the source page's stem and
/// appends `<DEDUP_FROM_TOKEN><src_workspace>` (then `-2`, `-3`, …). Used by
/// the copy-purge merge to keep BOTH pages when a path conflicts.
async fn dedup_dest_path(
    state: &AdminState,
    dst_ws: WorkspaceId,
    dst_proj: ProjectId,
    src_path: &str,
    src_workspace: &str,
    used: &std::collections::HashSet<String>,
) -> PagePath {
    let (stem, ext) = match src_path.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (src_path.to_string(), String::new()),
    };
    let slug: String = src_workspace
        .chars()
        .map(|c| if is_dedup_slug_char(c) { c } else { '-' })
        .collect();
    let base = format!("{stem}{DEDUP_FROM_TOKEN}{slug}");
    let mut n = 0u32;
    loop {
        let cand = if n == 0 {
            format!("{base}{ext}")
        } else {
            format!("{base}-{n}{ext}")
        };
        let collides = used.contains(&cand)
            || matches!(
                state.reader.page_body_by_ids(dst_ws, dst_proj, &cand).await,
                Ok(Some(_))
            );
        if !collides && let Ok(p) = PagePath::new(cand) {
            return p;
        }
        n += 1;
    }
}

fn page_copy_differs(
    existing: &ai_memory_store::StoredPageBody,
    source: &Markdown,
    source_title: &str,
    source_tier: Tier,
    source_pinned: bool,
) -> bool {
    let source_frontmatter = match serde_json::to_string(&source.frontmatter) {
        Ok(value) => value,
        Err(_) => return true,
    };
    existing.body != source.body
        || existing.frontmatter_json != source_frontmatter
        || existing.title != source_title
        || existing.tier != source_tier.as_str()
        || existing.pinned != source_pinned
}

async fn handle_move_project(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Json(req): Json<MoveProjectRequest>,
) -> impl IntoResponse {
    // Carry the authenticated actor into both legs (move admission + the
    // source-purge admission) so a scope-guard webhook authorizes by user.
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    match move_project_core(&state, &req, actor).await {
        Ok(report) => (
            StatusCode::OK,
            Json(serde_json::to_value(&report).unwrap_or_else(|_| serde_json::json!({}))),
        ),
        Err(resp) => resp,
    }
}

/// Validate one move request, pick the true-move vs copy-purge path, and
/// execute it — returning the report as data (or the HTTP error a handler
/// surfaces verbatim). Shared by `handle_move_project` (one move) and
/// `handle_merge_workspace` (a move per source project).
async fn move_project_core(
    state: &Arc<AdminState>,
    req: &MoveProjectRequest,
    actor: ai_memory_core::ActorContext,
) -> Result<MoveProjectReport, MoveErr> {
    // Destructive: it purges the source after copying. Require confirm.
    if !req.confirm {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "destructive operation requires confirm=true"
            })),
        ));
    }

    // A same-workspace "move" would get-or-create the SAME project as both
    // source and destination, copy it onto itself, then purge it — data
    // loss. Reject; in-workspace renames go through rename-project.
    if req.from_workspace == req.to_workspace {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "from_workspace and to_workspace are identical; use rename-project instead"
            })),
        ));
    }

    // Resolve the SOURCE without auto-creating — 404 on a typo.
    let (src_ws, src_proj) =
        lookup_ws_proj_no_create(state, &req.from_workspace, &req.project).await?;

    // Live-session guard: refuse to move the project the hook router is
    // currently writing to. A live session's next observation/log would carry
    // the now-stale workspace id (the (workspace_id, project_id) trigger would
    // make it fail, but the operator should consciously opt in). `force: true`
    // proceeds — safe because the move republishes the active pointer below.
    if !req.force && state.active_project.contains_project(src_proj) {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "{}/{} is the active session's project; a live move risks stale-cache writes. \
                     Re-run with force=true to proceed.",
                    req.from_workspace, req.project
                )
            })),
        ));
    }

    // Detect MERGE: does the destination workspace already hold a same-named
    // project? (find_workspace may be None when the dest ws doesn't exist yet.)
    let merged_into_existing = match state.reader.find_workspace(req.to_workspace.clone()).await {
        Ok(Some(dst_ws)) => matches!(
            state.reader.find_project(dst_ws, req.project.clone()).await,
            Ok(Some(_))
        ),
        Ok(None) => false,
        Err(e) => return Err(internal_err(e.to_string())),
    };

    let pre_checkpoint = checkpoint_or_500(
        &state.wiki,
        format!(
            "pre-move-project {}/{} -> {}/{}",
            req.from_workspace, req.project, req.to_workspace, req.project
        ),
    )?;

    // FRESH destination (no same-named project there) → lossless TRUE MOVE.
    // Re-stamp the source project's workspace_id across every domain table in
    // one transaction (same project_id), then rename its on-disk dir. This
    // keeps sessions/observations/handoffs and the full supersession history,
    // and is O(1) instead of O(pages) — no per-page re-write, re-embed, or
    // admission webhook. The copy+purge path below is reserved for the MERGE
    // case, where two project_ids can't be re-stamped into one
    // (UNIQUE(workspace_id, name) collision).
    if !merged_into_existing {
        return true_move_project(state, req, src_ws, src_proj, pre_checkpoint, actor).await;
    }

    // MERGE: the destination already holds a same-named project. Get-or-create
    // it (auto-creating the destination workspace) and copy the source's
    // latest pages into it, then purge the source.
    let (dst_ws, dst_proj) = create_ws_proj(state, &req.to_workspace, &req.project).await?;

    copy_purge_merge(
        state,
        req,
        src_ws,
        src_proj,
        dst_ws,
        dst_proj,
        pre_checkpoint,
        actor,
    )
    .await
}

// ---------------------------------------------------------------------
// move-session
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/move-session`.
///
/// Two shapes share the route: a single move names `session_id`; a batch
/// names `from_project` (plus optional `from_workspace`) and moves every
/// session touching that scope (a `sessions` row there OR observations
/// stamped into it). Exactly one of the two must be present. A session whose
/// row already sits in the destination is re-homed: only its rows still
/// lying elsewhere move (`session_moved: false`).
#[derive(Deserialize)]
struct MoveSessionRequest {
    /// The session to move (single form).
    #[serde(default)]
    session_id: Option<SessionId>,
    /// Source workspace of the batch form. Defaults to `default`.
    #[serde(default)]
    from_workspace: Option<String>,
    /// Source project of the batch form: every session touching it moves.
    #[serde(default)]
    from_project: Option<String>,
    /// Destination workspace. Defaults to the source workspace.
    #[serde(default)]
    workspace: Option<String>,
    /// Destination project. Must already exist unless `create` is set.
    project: String,
    /// What happens to `sessions/<id>.md`: `move` (default) carries the page
    /// and its history along; `regenerate` retires it in the source so the
    /// next consolidation writes a fresh one in the destination.
    #[serde(default)]
    pages: PagesMode,
    /// Without `confirm: true` the server runs the move inside a rolled-back
    /// transaction and reports what would change (`dry_run: true`).
    #[serde(default)]
    confirm: bool,
    /// Skip the live-session guards: an open session (no session end
    /// recorded; not applied to a re-home, whose row does not move), a
    /// `pending`/`running` consolidation job, and, for the batch form, the
    /// hook router's active-project pointer.
    #[serde(default)]
    force: bool,
    /// Create the destination workspace/project when absent instead of 404.
    /// A dry run never creates: it reports `would_create_project: true` and
    /// the counts computed against an absent target.
    #[serde(default)]
    create: bool,
}

/// `workspace/project` pair as names, for report labels.
#[derive(Debug, Clone, Serialize)]
pub struct ScopeLabel {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
}

/// Row counts of one session move; identical for the dry run.
#[derive(Debug, Clone, Serialize)]
pub struct MoveSessionCounts {
    /// `observations` rows re-stamped.
    pub observations: u64,
    /// `handoffs` rows (produced by the session) re-stamped.
    pub handoffs: u64,
    /// `session_consolidation_jobs` rows re-stamped.
    pub consolidation_jobs: u64,
    /// `auto_improve_runs` rows re-stamped.
    pub auto_improve_runs: u64,
    /// `auto_improve_scheduler_claims` rows re-stamped.
    pub auto_improve_claims: u64,
    /// `pages` versions of `sessions/<id>.md` re-stamped (`pages: move`).
    pub page_versions_moved: u64,
    /// `pages` rows of `sessions/<id>.md` retired (`pages: regenerate`).
    pub pages_regenerated: u64,
}

/// One scope a move gathers observations out of, for the operator's report.
#[derive(Debug, Serialize)]
pub struct MoveSessionScopeCount {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Observations of the session currently stamped into that scope.
    pub observations: u64,
}

/// Wire-format report for one session in `POST /admin/move-session`.
#[derive(Debug, Serialize)]
pub struct MoveSessionReport {
    /// The moved session.
    pub session_id: String,
    /// `true` when nothing was written (no `confirm`).
    pub dry_run: bool,
    /// Whether the `sessions` row changed scope. `false` for a re-home: the
    /// row already sat in the destination and only stray rows (the counts
    /// below) were gathered into it.
    pub session_moved: bool,
    /// Dry run with `create` against a destination that does not exist yet:
    /// the confirm run will create it. Never set on a confirmed move.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub would_create_project: bool,
    /// Every scope holding observations of this session other than the
    /// destination, with counts. More than one entry — or one that is not
    /// `from` — means the move drains a project the caller did not name.
    /// Empty when everything already sits in the destination.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub source_scopes: Vec<MoveSessionScopeCount>,
    /// Scope the session came from.
    pub from: ScopeLabel,
    /// Scope the session now belongs to.
    pub to: ScopeLabel,
    /// Row counts.
    pub summary: MoveSessionCounts,
    /// What happened (or would happen) to `sessions/<id>.md`: `"moved"`,
    /// `"regenerated"`, `"already in destination"` (re-home whose page rows
    /// all sit in the destination), or `"none"` when the session has no page
    /// anywhere.
    pub page: &'static str,
    /// The session's recorded working directory, left untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Set when `basename(cwd)` differs from the destination project name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_warning: Option<String>,
    /// Pre-move checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-move checkpoint, if the move changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// Wire-format report for the batch form of `POST /admin/move-session`.
#[derive(Debug, Serialize)]
pub struct MoveSessionBatchReport {
    /// `true` when nothing was written (no `confirm`).
    pub dry_run: bool,
    /// Dry run with `create` against a destination that does not exist yet.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub would_create_project: bool,
    /// Source scope.
    pub from: ScopeLabel,
    /// Destination scope.
    pub to: ScopeLabel,
    /// Sessions touching the source scope (a `sessions` row there or at
    /// least one observation stamped into it).
    pub total: usize,
    /// Sessions moved or re-homed (or, for a dry run, that would be).
    pub moved: usize,
    /// One report per session, oldest first (row `started_at` or first
    /// observation in the source).
    pub sessions: Vec<MoveSessionReport>,
    /// Pre-move checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_checkpoint: Option<String>,
    /// Post-move checkpoint, if the batch changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
}

/// One session move, resolved and validated, before any guard runs.
struct MoveSessionPlan {
    session_id: SessionId,
    /// Scope the rows are gathered from: the session row's own scope in the
    /// single form, the batch source in the batch form (where the row may
    /// well sit elsewhere, even in `to`).
    from: (WorkspaceId, ProjectId),
    from_label: ScopeLabel,
    to: MoveTarget,
    to_label: ScopeLabel,
    /// Where the `sessions` row sits right now.
    row_scope: (WorkspaceId, ProjectId),
    cwd: Option<String>,
}

impl MoveSessionPlan {
    /// The row already sits in the destination: only stray rows move.
    fn is_rehome(&self) -> bool {
        self.to.existing() == Some(self.row_scope)
    }
}

/// The destination of a move: an existing scope, or (dry run with `create`
/// only) a scope that does not exist yet and would be created on confirm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MoveTarget {
    Existing((WorkspaceId, ProjectId)),
    WouldCreate,
}

impl MoveTarget {
    fn existing(self) -> Option<(WorkspaceId, ProjectId)> {
        match self {
            Self::Existing(scope) => Some(scope),
            Self::WouldCreate => None,
        }
    }
}

async fn handle_move_session(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    author_ext: Option<axum::Extension<ai_memory_core::UserId>>,
    Json(req): Json<MoveSessionRequest>,
) -> Response {
    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    let author_id = author_ext.map(|axum::Extension(u)| u);
    match (req.session_id, req.from_project.as_deref()) {
        (Some(session_id), None) => {
            match move_single_session(&state, &req, session_id, author_id, actor).await {
                Ok(report) => (StatusCode::OK, Json(json_or_empty(&report))).into_response(),
                Err(resp) => resp.into_response(),
            }
        }
        (None, Some(from_project)) => {
            move_session_batch(&state, &req, from_project, author_id, actor).await
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "provide exactly one of session_id (single move) or from_project (batch)"
            })),
        )
            .into_response(),
    }
}

fn json_or_empty<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or_else(|_| serde_json::json!({}))
}

/// Best-effort names for a scope; ids stand in when a row vanished
/// underneath us (a concurrent purge), so a report is still produced.
async fn scope_label(state: &AdminState, ws: WorkspaceId, proj: ProjectId) -> ScopeLabel {
    let workspace = state
        .reader
        .workspace_name_by_id(ws)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| ws.to_string());
    let project = state
        .reader
        .project_name_by_id(ws, proj)
        .await
        .ok()
        .flatten()
        .unwrap_or_else(|| proj.to_string());
    ScopeLabel { workspace, project }
}

/// Resolve the destination scope: existing only (404) unless `create`. A
/// dry run never writes, so with `create` and no `confirm` an absent
/// destination resolves to [`MoveTarget::WouldCreate`] instead of a row.
async fn resolve_move_session_target(
    state: &AdminState,
    workspace: &str,
    project: &str,
    create: bool,
    confirm: bool,
) -> Result<MoveTarget, MoveErr> {
    if create && confirm {
        return create_ws_proj(state, workspace, project)
            .await
            .map(MoveTarget::Existing);
    }
    match lookup_existing_scope(&state.reader, workspace, project).await {
        Ok(scope) => Ok(MoveTarget::Existing(scope.as_tuple())),
        Err(e) if create && e.is_not_found() => Ok(MoveTarget::WouldCreate),
        Err(e) => Err(scope_err(e)),
    }
}

/// Dry-run counts of a move into a destination that does not exist yet:
/// nothing of the session can already be there, so every row keyed to it
/// moves, and the page rows are those of `from` (moved or retired per
/// `pages`). Read-only; the store never sees the absent target.
async fn preview_move_into_absent_target(
    reader: &ReaderPool,
    session_id: SessionId,
    from: (WorkspaceId, ProjectId),
    pages: PagesMode,
) -> Result<MoveSessionCounts, StoreError> {
    let rows = reader.session_dependent_rows(session_id, from).await?;
    let (page_versions_moved, pages_regenerated) = match pages {
        PagesMode::Move => (rows.page_versions, 0),
        PagesMode::Regenerate => (0, rows.page_latest),
    };
    Ok(MoveSessionCounts {
        observations: rows.observations,
        handoffs: rows.handoffs,
        consolidation_jobs: rows.consolidation_jobs,
        auto_improve_runs: rows.auto_improve_runs,
        auto_improve_claims: rows.auto_improve_claims,
        page_versions_moved,
        pages_regenerated,
    })
}

/// `(session still open, consolidation job pending or running)` for the
/// live-session guards.
async fn session_move_blockers(
    reader: &ReaderPool,
    session_id: SessionId,
) -> Result<(bool, bool), StoreError> {
    reader
        .with_conn(move |conn| {
            let open: bool = conn.query_row(
                "SELECT ended_at IS NULL FROM sessions WHERE id = ?1",
                [session_id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            let pending: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM session_consolidation_jobs \
                 WHERE session_id = ?1 AND state IN ('pending', 'running'))",
                [session_id.as_bytes().as_slice()],
                |row| row.get(0),
            )?;
            Ok((open, pending))
        })
        .await
}

/// `(session_id, row scope, cwd)` of every session touching a scope (a
/// `sessions` row there or observations stamped into it), oldest first. The
/// row scope may differ from the queried scope: that is the phantom-bucket
/// case the batch re-homes.
async fn list_scope_sessions(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> Result<Vec<(SessionId, (WorkspaceId, ProjectId), Option<String>)>, StoreError> {
    let ids = reader
        .session_ids_touching_scope(workspace_id, project_id)
        .await?;
    let mut out = Vec::with_capacity(ids.len());
    for sid in ids {
        // Observations reference `sessions` (FK on), so a listed id always
        // has a row; a concurrent purge is the only way to lose it here.
        if let Some((ws, proj, cwd)) = reader.find_session_scope(sid).await? {
            out.push((sid, (ws, proj), cwd));
        }
    }
    Ok(out)
}

fn move_session_store_err(e: StoreError) -> MoveErr {
    match e {
        StoreError::NotFound(msg) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": msg })),
        ),
        e @ StoreError::PagePathTaken { .. } => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "{e}; re-run with pages=regenerate or resolve the destination page first"
                )
            })),
        ),
        other => internal_err(other.to_string()),
    }
}

fn move_session_wiki_err(e: WikiError) -> MoveErr {
    match e {
        WikiError::Store(store_err) => move_session_store_err(store_err),
        e @ WikiError::DestinationPageExists(_) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": e.to_string() })),
        ),
        other => internal_err(format!("move-session aborted: {other}")),
    }
}

/// The session's cwd is historical truth and stays put; say so when the
/// hook router's basename heuristic would not map it to the destination.
fn move_session_cwd_warning(cwd: Option<&str>, to_project: &str) -> Option<String> {
    let cwd = cwd?;
    let base = std::path::Path::new(cwd).file_name()?.to_str()?;
    if base == to_project {
        return None;
    }
    Some(format!(
        "session cwd '{cwd}' resolves to project '{base}' by basename, not '{to_project}'; \
         the cwd was left as recorded. New sessions started there still land in '{base}' \
         unless a .ai-memory.toml marker pins project = \"{to_project}\"; with \
         [routing] mid_session = \"sticky\" the remaining events of this session follow it."
    ))
}

fn move_session_source_scopes(
    summary: &ai_memory_store::MoveSessionSummary,
) -> Vec<MoveSessionScopeCount> {
    summary
        .source_scopes
        .iter()
        .map(|scope| MoveSessionScopeCount {
            workspace: scope.workspace_name.clone(),
            project: scope.project_name.clone(),
            observations: scope.observations,
        })
        .collect()
}

fn move_session_counts(summary: &ai_memory_store::MoveSessionSummary) -> MoveSessionCounts {
    MoveSessionCounts {
        observations: summary.observations,
        handoffs: summary.handoffs,
        consolidation_jobs: summary.consolidation_jobs,
        auto_improve_runs: summary.auto_improve_runs,
        auto_improve_claims: summary.auto_improve_claims,
        page_versions_moved: summary.page_versions_moved,
        pages_regenerated: summary.pages_regenerated,
    }
}

fn move_session_page_label(
    pages: PagesMode,
    touched_rows: bool,
    touched_file: bool,
    already_in_target: bool,
) -> &'static str {
    match (pages, touched_rows || touched_file) {
        (_, false) if already_in_target => "already in destination",
        (_, false) => "none",
        (PagesMode::Move, true) => "moved",
        (PagesMode::Regenerate, true) => "regenerated",
    }
}

/// Guards, then the dry run or the real move of one planned session. The
/// caller wraps the call in the pre/post checkpoints so a batch takes one
/// pair, not one per session.
async fn move_planned_session(
    state: &Arc<AdminState>,
    req: &MoveSessionRequest,
    plan: MoveSessionPlan,
    author_id: Option<ai_memory_core::UserId>,
    actor: &ai_memory_core::ActorContext,
) -> Result<MoveSessionReport, MoveErr> {
    let sid = plan.session_id;
    let rehome = plan.is_rehome();

    // Live-session guards. An open session may still receive events, and a
    // queued consolidation would write the page under the old scope; both
    // are the operator's call with `force`. A re-home leaves the row where
    // it is, so an open session is no reason to refuse gathering its strays.
    let (open, pending_job) = session_move_blockers(&state.reader, sid)
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    if !req.force {
        if open && !rehome {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!(
                        "session {sid} is still open (no session end recorded); \
                         finish it or force the move (--force / force: true)"
                    )
                })),
            ));
        }
        if pending_job {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!(
                        "session {sid} has a pending or running consolidation job; \
                         wait for it or force the move (--force / force: true)"
                    )
                })),
            ));
        }
    }

    let cwd_warning = move_session_cwd_warning(plan.cwd.as_deref(), &plan.to_label.project);
    let file_name = format!("{sid}.md");
    let src_file = state
        .wiki
        .project_root(plan.from.0, plan.from.1)
        .join("sessions")
        .join(&file_name);

    // Dry run with `create` against an absent destination: nothing exists to
    // collide with, and the store cannot re-stamp into a scope that has no
    // row, so the counts come from a read-only preview.
    let to = match plan.to {
        MoveTarget::Existing(to) => to,
        MoveTarget::WouldCreate => {
            debug_assert!(
                !req.confirm,
                "an absent target is only planned for a dry run"
            );
            let counts = preview_move_into_absent_target(&state.reader, sid, plan.from, req.pages)
                .await
                .map_err(|e| internal_err(e.to_string()))?;
            let touched_rows = counts.page_versions_moved > 0 || counts.pages_regenerated > 0;
            return Ok(MoveSessionReport {
                session_id: sid.to_string(),
                dry_run: true,
                session_moved: true,
                would_create_project: true,
                source_scopes: Vec::new(),
                from: plan.from_label,
                to: plan.to_label,
                page: move_session_page_label(req.pages, touched_rows, src_file.is_file(), false),
                summary: counts,
                cwd: plan.cwd,
                cwd_warning,
                pre_checkpoint: None,
                checkpoint: None,
            });
        }
    };

    // A page file already at the destination blocks a page move; checked here
    // too so the dry run predicts what the wiki re-checks under its lock. When
    // the source scope IS the destination (single-form re-home) there is no
    // file to move and the wiki skips the file step too.
    let dst_file = state
        .wiki
        .project_root(to.0, to.1)
        .join("sessions")
        .join(&file_name);
    let file_moves = plan.from != to && src_file.is_file();
    if req.pages == PagesMode::Move && file_moves && dst_file.exists() {
        return Err(move_session_wiki_err(WikiError::DestinationPageExists(
            dst_file.display().to_string(),
        )));
    }

    if !req.confirm {
        let summary = state
            .writer
            .move_session(sid, to.0, to.1, req.pages, author_id, false)
            .await
            .map_err(move_session_store_err)?;
        let touched_rows = summary.page_versions_moved > 0 || summary.pages_regenerated > 0;
        return Ok(MoveSessionReport {
            session_id: sid.to_string(),
            dry_run: true,
            session_moved: summary.session_moved,
            would_create_project: false,
            source_scopes: move_session_source_scopes(&summary),
            from: plan.from_label,
            to: plan.to_label,
            summary: move_session_counts(&summary),
            page: move_session_page_label(
                req.pages,
                touched_rows,
                file_moves,
                summary.page_versions_in_target > 0,
            ),
            cwd: plan.cwd,
            cwd_warning,
            pre_checkpoint: None,
            checkpoint: None,
        });
    }

    let move_ctx = AdmissionContext {
        workspace: plan.from_label.workspace.clone(),
        project: plan.from_label.project.clone(),
        destination_workspace: Some(plan.to_label.workspace.clone()),
        destination_project: Some(plan.to_label.project.clone()),
        op: AdmissionOp::MoveSession,
        actor: actor.clone(),
        ..Default::default()
    };
    // The wiki owns the critical section: admission, file move, SQL
    // re-stamp, and the file rollback on SQL failure.
    let outcome = state
        .wiki
        .move_session_page(sid, plan.from, to, req.pages, author_id, Some(move_ctx))
        .await
        .map_err(move_session_wiki_err)?;
    let touched_rows =
        outcome.summary.page_versions_moved > 0 || outcome.summary.pages_regenerated > 0;
    Ok(MoveSessionReport {
        session_id: sid.to_string(),
        dry_run: false,
        session_moved: outcome.summary.session_moved,
        would_create_project: false,
        source_scopes: move_session_source_scopes(&outcome.summary),
        from: plan.from_label,
        to: plan.to_label,
        summary: move_session_counts(&outcome.summary),
        page: move_session_page_label(
            req.pages,
            touched_rows,
            outcome.file != SessionPageFile::Absent,
            outcome.summary.page_versions_in_target > 0,
        ),
        cwd: plan.cwd,
        cwd_warning,
        pre_checkpoint: None,
        checkpoint: None,
    })
}

async fn move_single_session(
    state: &Arc<AdminState>,
    req: &MoveSessionRequest,
    session_id: SessionId,
    author_id: Option<ai_memory_core::UserId>,
    actor: ai_memory_core::ActorContext,
) -> Result<MoveSessionReport, MoveErr> {
    let (from_ws, from_proj, cwd) = state
        .reader
        .find_session_scope(session_id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("session {session_id} not found") })),
            )
        })?;
    let from_label = scope_label(state, from_ws, from_proj).await;
    let to_workspace = req
        .workspace
        .clone()
        .unwrap_or_else(|| from_label.workspace.clone());
    let to =
        resolve_move_session_target(state, &to_workspace, &req.project, req.create, req.confirm)
            .await?;
    let plan = MoveSessionPlan {
        session_id,
        from: (from_ws, from_proj),
        from_label,
        to,
        to_label: ScopeLabel {
            workspace: to_workspace,
            project: req.project.clone(),
        },
        row_scope: (from_ws, from_proj),
        cwd,
    };

    if !req.confirm {
        return move_planned_session(state, req, plan, author_id, &actor).await;
    }
    let label = format!(
        "move-session {session_id} {}/{} -> {}/{}",
        plan.from_label.workspace,
        plan.from_label.project,
        plan.to_label.workspace,
        plan.to_label.project
    );
    let pre_checkpoint = checkpoint_or_500(&state.wiki, format!("pre-{label}"))?;
    let mut report = move_planned_session(state, req, plan, author_id, &actor).await?;
    if let Err(e) = state.wiki.backfill_scope_manifests().await {
        warn!(error = %e, "move-session: scope-manifest backfill failed after move");
    }
    report.pre_checkpoint = pre_checkpoint;
    report.checkpoint = checkpoint_or_warn(&state.wiki, label);
    Ok(report)
}

async fn move_session_batch(
    state: &Arc<AdminState>,
    req: &MoveSessionRequest,
    from_project: &str,
    author_id: Option<ai_memory_core::UserId>,
    actor: ai_memory_core::ActorContext,
) -> Response {
    let from_workspace = req
        .from_workspace
        .clone()
        .unwrap_or_else(|| DEFAULT_WORKSPACE_NAME.to_string());
    let from = match lookup_ws_proj_no_create(state, &from_workspace, from_project).await {
        Ok(ids) => ids,
        Err(e) => return e.into_response(),
    };
    let to_workspace = req
        .workspace
        .clone()
        .unwrap_or_else(|| from_workspace.clone());
    let to = match resolve_move_session_target(
        state,
        &to_workspace,
        &req.project,
        req.create,
        req.confirm,
    )
    .await
    {
        Ok(target) => target,
        Err(e) => return e.into_response(),
    };
    let from_label = ScopeLabel {
        workspace: from_workspace,
        project: from_project.to_string(),
    };
    let to_label = ScopeLabel {
        workspace: to_workspace,
        project: req.project.clone(),
    };
    if to.existing() == Some(from) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "source and destination scopes are identical"
            })),
        )
            .into_response();
    }
    // Same guard as move-project: the project the hook router is writing to
    // is not emptied out from under it without an explicit `force`.
    if !req.force && state.active_project.contains_project(from.1) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!(
                    "{}/{} is the active session's project; force the move (--force / \
                     force: true) to move its sessions anyway",
                    from_label.workspace, from_label.project
                )
            })),
        )
            .into_response();
    }
    let sessions = match list_scope_sessions(&state.reader, from.0, from.1).await {
        Ok(v) => v,
        Err(e) => return internal_err(e.to_string()).into_response(),
    };

    let label = format!(
        "move-session {}/{} -> {}/{}",
        from_label.workspace, from_label.project, to_label.workspace, to_label.project
    );
    let pre_checkpoint = if req.confirm && !sessions.is_empty() {
        match checkpoint_or_500(&state.wiki, format!("pre-{label}")) {
            Ok(oid) => oid,
            Err(e) => return e.into_response(),
        }
    } else {
        None
    };

    let total = sessions.len();
    let mut reports = Vec::with_capacity(total);
    let mut failure: Option<(SessionId, MoveErr)> = None;
    for (session_id, row_scope, cwd) in sessions {
        // The page file follows the page rows: a session rooted in a third
        // scope (only some observations in the source) is moved whole from
        // where its row sits; a session already rooted in the destination is
        // re-homed and its stray page file, if any, is looked for in the
        // source.
        let (plan_from, plan_from_label) = if row_scope == from || to.existing() == Some(row_scope)
        {
            (from, from_label.clone())
        } else {
            (
                row_scope,
                scope_label(state, row_scope.0, row_scope.1).await,
            )
        };
        let plan = MoveSessionPlan {
            session_id,
            from: plan_from,
            from_label: plan_from_label,
            to,
            to_label: to_label.clone(),
            row_scope,
            cwd,
        };
        match move_planned_session(state, req, plan, author_id, &actor).await {
            Ok(report) => reports.push(report),
            Err(e) => {
                failure = Some((session_id, e));
                break;
            }
        }
    }

    let checkpoint = if req.confirm && !reports.is_empty() {
        if let Err(e) = state.wiki.backfill_scope_manifests().await {
            warn!(error = %e, "move-session: scope-manifest backfill failed after batch");
        }
        checkpoint_or_warn(&state.wiki, label)
    } else {
        None
    };

    // Stop at the first error, but say how far the batch got: the sessions
    // already moved stay moved (each was its own transaction).
    if let Some((session_id, (status, Json(mut body)))) = failure {
        if let Some(obj) = body.as_object_mut() {
            obj.insert(
                "session_id".into(),
                serde_json::json!(session_id.to_string()),
            );
            obj.insert("dry_run".into(), serde_json::json!(!req.confirm));
            obj.insert("total".into(), serde_json::json!(total));
            obj.insert("moved".into(), serde_json::json!(reports.len()));
            obj.insert("sessions".into(), json_or_empty(&reports));
            if let Some(oid) = checkpoint {
                obj.insert("checkpoint".into(), serde_json::json!(oid));
            }
        }
        return (status, Json(body)).into_response();
    }
    let report = MoveSessionBatchReport {
        dry_run: !req.confirm,
        would_create_project: to == MoveTarget::WouldCreate,
        from: from_label,
        to: to_label,
        total,
        moved: reports.len(),
        sessions: reports,
        pre_checkpoint,
        checkpoint,
    };
    (StatusCode::OK, Json(json_or_empty(&report))).into_response()
}

/// One source page, with everything the copy loop and the block
/// pre-check both need. Built in ONE pass over the source listing so
/// the previous two-pass implementation's double-IO (pre-scan + copy
/// loop each calling `page_meta` + `page_body_by_ids` + `read_page`)
/// collapses to a single read per page.
struct PreparedSourcePage {
    path: PagePath,
    /// Path as a String — only needed for reports / `pages_skipped`
    /// and `dedup` lookups; kept here so we don't keep recomputing.
    path_str: String,
    title: String,
    tier: Tier,
    pinned: bool,
    /// `Some(md)` when the on-disk source survived parsing; `None` when
    /// reading or parsing failed and the page must be skipped (the
    /// safety guard at the end of the merge will refuse to purge).
    md: Option<Markdown>,
    /// `true` when the destination already holds this path with a
    /// DIFFERENT page (body / frontmatter / title / tier / pinned).
    /// `false` when there's no destination row, the destination
    /// matches verbatim (no-op supersession), or the source itself
    /// couldn't be parsed and we'd skip it anyway.
    dest_conflict: bool,
}

/// Load every source page (metadata + body) and pre-classify whether
/// its path collides with a different page at the destination. Both
/// the `Block` pre-check and the copy loop drive off the returned
/// vector, so each page is read at most once.
async fn prepare_source_pages(
    state: &AdminState,
    req: &MoveProjectRequest,
    src_ws: WorkspaceId,
    src_proj: ProjectId,
    dst_ws: WorkspaceId,
    dst_proj: ProjectId,
    summaries: &[ai_memory_store::PageSummary],
) -> Vec<PreparedSourcePage> {
    let mut out = Vec::with_capacity(summaries.len());
    for s in summaries {
        let Ok(path) = PagePath::new(s.path.clone()) else {
            // Unparseable path — record a "skip" entry so the copy
            // loop can report it without re-trying the lookup.
            out.push(PreparedSourcePage {
                path: PagePath::new("invalid.md".to_string()).expect("valid placeholder"),
                path_str: s.path.clone(),
                title: s.title.clone(),
                tier: Tier::Semantic,
                pinned: false,
                md: None,
                dest_conflict: false,
            });
            continue;
        };
        let tier: Tier = s.tier.parse().unwrap_or(Tier::Semantic);
        let pinned = matches!(
            state.reader.page_meta(&req.from_workspace, &req.project, &s.path).await,
            Ok(Some(ref m)) if m.pinned
        );
        let md = state.wiki.read_page(src_ws, src_proj, &path).ok();
        // Compute the conflict decision once. The check is "is there a
        // DIFFERENT page at this path?", which requires both the dest
        // body and the source markdown — when either is missing, treat
        // as no-conflict and let the copy loop's natural-path branch
        // handle it (it'll either supersede a no-op or skip with a
        // missing-md guard).
        let dest_conflict = matches!(
            (
                state
                    .reader
                    .page_body_by_ids(dst_ws, dst_proj, s.path.as_str())
                    .await,
                &md,
            ),
            (Ok(Some(existing)), Some(md_ref))
                if page_copy_differs(&existing, md_ref, &s.title, tier, pinned)
        );
        out.push(PreparedSourcePage {
            path,
            path_str: s.path.clone(),
            title: s.title.clone(),
            tier,
            pinned,
            md,
            dest_conflict,
        });
    }
    out
}

/// Execute the copy-purge merge once the destination has been
/// resolved. Pulled out of `handle_move_project` so the orchestrator
/// reads as "validate → branch → copy_purge_merge", and the copy
/// loop's per-page IO runs through a pre-computed `PreparedSourcePage`
/// instead of fetching the same metadata twice.
// Merge-path orchestration: state + req + the four namespaced ids
// (src/dst ws+proj) + pre-checkpoint + the authenticated actor. Bundling
// the ids into a struct would obscure more than the arity warning, matching
// the existing `#[allow]`s on the equivalent store-layer helpers.
#[allow(clippy::too_many_arguments)]
async fn copy_purge_merge(
    state: &AdminState,
    req: &MoveProjectRequest,
    src_ws: WorkspaceId,
    src_proj: ProjectId,
    dst_ws: WorkspaceId,
    dst_proj: ProjectId,
    pre_checkpoint: Option<String>,
    actor: ai_memory_core::ActorContext,
) -> Result<MoveProjectReport, MoveErr> {
    // Enumerate the source's latest pages (authoritative on is_latest).
    let summaries = match state
        .reader
        .list_pages(&req.from_workspace, &req.project)
        .await
    {
        Ok(s) => s,
        Err(e) => return Err(internal_err(e.to_string())),
    };

    // Single pass over the source: load each page's metadata + body
    // AND classify the destination conflict, so the block pre-check
    // and the copy loop don't re-query the same rows.
    let prepared =
        prepare_source_pages(state, req, src_ws, src_proj, dst_ws, dst_proj, &summaries).await;

    // Under the default `block` policy, abort the WHOLE move now —
    // before anything is copied — so the source stays intact and the
    // operator resolves the conflicts or re-runs with an explicit
    // overwrite/duplicate. Drives off the cached `dest_conflict_body`
    // computed by `prepare_source_pages`.
    if req.on_conflict == OnConflict::Block {
        let blocking: Vec<String> = prepared
            .iter()
            .filter(|p| p.dest_conflict)
            .map(|p| p.path_str.clone())
            .collect();
        if !blocking.is_empty() {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "destination already has pages at these paths with different content; \
                              resolve them or re-run with on_conflict=overwrite or on_conflict=duplicate",
                    "conflicts": blocking,
                })),
            ));
        }
    }

    // Carry the source page embeddings over verbatim instead of recomputing
    // them — embedding is the dominant per-page cost of a bulk move. Writes go
    // through an embedder-less Wiki so `write_page` never re-embeds; we then
    // store each source vector against the new page id. Only embeddings
    // computed with the CURRENTLY configured embedder ({provider,model,dim})
    // are loaded (load_embeddings filters on them), so the "one model per
    // index" invariant holds; any page lacking a current-model embedding is
    // simply copied without one (backfill later via `ai-memory embed`).
    let copy_wiki = state.wiki.clone().without_embedder();
    let mut src_embeddings: std::collections::HashMap<String, Vec<u8>> =
        std::collections::HashMap::new();
    let embed_meta: Option<(String, String, u32)> = if let Some(embedder) = &state.embedder {
        let (provider, model, dim) = (
            embedder.provider().to_string(),
            embedder.model().to_string(),
            embedder.dim(),
        );
        match state
            .reader
            .load_embeddings(src_ws, src_proj, provider.clone(), model.clone(), dim)
            .await
        {
            Ok(rows) => {
                for e in rows {
                    src_embeddings.insert(e.path.to_string(), f32_vec_to_bytes(&e.vector));
                }
                Some((provider, model, dim))
            }
            Err(e) => {
                warn!(error = %e, "move-project: failed to load source embeddings; copies land without vectors");
                None
            }
        }
    } else {
        None
    };

    // COPY each page through the (embedder-less) write path so sanitization,
    // link re-resolution, FTS upsert (and admission/git-mirror on deploy) all
    // fire — minus the per-page embed, which we carry over below. Drives off
    // the prebuilt `PreparedSourcePage` vec so each page is read at most
    // once across the whole merge (the previous version re-fetched in the
    // pre-scan + the copy loop for Block-policy callers).
    let mut pages_copied = 0u64;
    let mut pages_skipped: Vec<String> = Vec::new();
    let mut conflicts: Vec<PathConflict> = Vec::new();
    let mut used_dest_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in prepared {
        let Some(md) = p.md else {
            // Either the path was unparseable or `read_page` failed —
            // either way we cannot copy it. The presence of a skip
            // here aborts the purge below.
            warn!(
                path = %p.path_str,
                "move-project: source page unreadable or invalid path; skipping"
            );
            pages_skipped.push(p.path_str.clone());
            continue;
        };
        // Apply the on_conflict policy using the cached classification.
        // `block` was already handled by the pre-scan above so we never
        // get here with `dest_conflict == true` AND `policy == Block`.
        let dest_path = if p.dest_conflict {
            match req.on_conflict {
                OnConflict::Duplicate => {
                    let deduped = dedup_dest_path(
                        state,
                        dst_ws,
                        dst_proj,
                        &p.path_str,
                        &req.from_workspace,
                        &used_dest_paths,
                    )
                    .await;
                    conflicts.push(PathConflict {
                        path: p.path_str.clone(),
                        moved_to: deduped.as_str().to_string(),
                    });
                    deduped
                }
                OnConflict::Overwrite => {
                    conflicts.push(PathConflict {
                        path: p.path_str.clone(),
                        moved_to: p.path_str.clone(),
                    });
                    p.path.clone()
                }
                OnConflict::Block => p.path.clone(), // unreachable
            }
        } else if used_dest_paths.contains(p.path_str.as_str()) {
            // The natural path is already claimed by an earlier
            // de-duplicated page; pick another to avoid clobbering it.
            let deduped = dedup_dest_path(
                state,
                dst_ws,
                dst_proj,
                &p.path_str,
                &req.from_workspace,
                &used_dest_paths,
            )
            .await;
            conflicts.push(PathConflict {
                path: p.path_str.clone(),
                moved_to: deduped.as_str().to_string(),
            });
            deduped
        } else {
            p.path.clone()
        };
        used_dest_paths.insert(dest_path.as_str().to_string());
        let new_page_id = match copy_wiki
            .write_page(WritePageRequest {
                workspace_id: dst_ws,
                project_id: dst_proj,
                path: dest_path.clone(),
                frontmatter: md.frontmatter,
                body: md.body,
                tier: p.tier,
                pinned: p.pinned,
                // Preserve the stored title verbatim (PageSummary.title is
                // the DB-derived title), rather than re-deriving it.
                title: Some(p.title.clone()),
                // Skip the named contributors enrich webhook on move copies:
                // the frontmatter (including the contributors list) is copied
                // verbatim, so re-enriching each copy adds nothing but blocking
                // per-page latency to a bulk move. write_page still resolves
                // the destination NAMES and runs the other hooks (git-mirror),
                // so the copy lands under the destination path.
                admission_ctx: Some(AdmissionContext {
                    skip_webhooks: vec![CONTRIBUTORS_WEBHOOK_NAME.to_string()],
                    ..AdmissionContext::default()
                }),
                author_id: None,
                actor: actor.clone(),
            })
            .await
        {
            Ok(pid) => pid,
            // ANY copy failure aborts BEFORE the purge — the source survives.
            Err(e) => return Err(internal_err(format!("copy of {} failed: {e}", p.path_str))),
        };
        // Carry the source embedding over (skip the re-embed) when the source
        // had one for the current model.
        if let (Some((provider, model, dim)), Some(bytes)) =
            (&embed_meta, src_embeddings.get(&p.path_str))
            && let Err(e) = state
                .writer
                .store_embedding(
                    new_page_id,
                    bytes.clone(),
                    provider.clone(),
                    model.clone(),
                    *dim,
                )
                .await
        {
            warn!(path = %p.path_str, error = %e, "move-project: failed to carry embedding; page copied without it");
        }
        pages_copied += 1;
    }

    // Safety: a skipped (unreadable) source page blocks the purge — purging
    // now would destroy data we failed to copy. Report and let the operator
    // fix + re-run (re-running is idempotent: copied pages just supersede).
    if !pages_skipped.is_empty() {
        let checkpoint = checkpoint_or_warn(
            &state.wiki,
            format!(
                "move-project-partial {}/{} -> {}/{}",
                req.from_workspace, req.project, req.to_workspace, req.project
            ),
        );
        let report = MoveProjectReport {
            from: format!("{}/{}", req.from_workspace, req.project),
            to: format!("{}/{}", req.to_workspace, req.project),
            // copy_purge_merge is only reached from the merge branch
            // of handle_move_project; the destination project pre-existed.
            merged_into_existing: true,
            moved_via: "copy-purge",
            pages_copied,
            workstreams_moved: 0,
            pages_skipped,
            source_purged: false,
            source_pages_deleted: 0,
            source_sessions_deleted: 0,
            source_observations_deleted: 0,
            source_handoffs_deleted: 0,
            source_embeddings_deleted: 0,
            files_deleted: Vec::new(),
            files_failed: Vec::new(),
            conflicts,
            pre_checkpoint,
            checkpoint,
        };
        return Ok(report);
    }

    // PURGE the source — only reached when every page copied successfully.
    // Admission must run BEFORE the DB destruction so a `failure_policy =
    // reject` webhook can still abort the second leg of the move while the
    // source is intact (mirrors `handle_purge_project`'s ordering). The
    // previous version called `Wiki::purge_project` AFTER `writer.purge_project`,
    // which ran admit AFTER the rows were gone — reject came too late.
    let label = format!("{}/{}", req.from_workspace, req.project);
    let purge_ctx = AdmissionContext {
        workspace: req.from_workspace.clone(),
        project: req.project.clone(),
        op: AdmissionOp::PurgeProject,
        actor,
        ..Default::default()
    };
    let resolved_purge_ctx = match state
        .wiki
        .admit_purge_project(src_ws, src_proj, Some(purge_ctx))
        .await
    {
        Ok(ctx) => ctx,
        Err(e) => return Err(internal_err(e.to_string())),
    };

    // The source purge is an internal step of move-project (a distinct op that
    // records its own move report); it is not attributed as a standalone
    // `purge_project` here, so the audit author is left NULL.
    // `Skip`: compaction is an operator's explicit choice on a destructive
    // command, not a side effect of moving a project. A `VACUUM` here would
    // rewrite the whole database in the middle of a move the caller asked to
    // be cheap.
    let summary = match state
        .writer
        .purge_project(
            src_ws,
            src_proj,
            &label,
            None,
            false,
            ai_memory_store::Compaction::Skip,
        )
        .await
    {
        Ok(s) => s,
        // A live managed run under the source always blocks the destructive
        // second leg. `force` only overrides the active-project guard; unlike
        // a true move, copy-purge would delete the lease and strand the raw
        // transcript. The pages are already copied, so a later retry is
        // idempotent and leaves the source intact meanwhile.
        Err(e @ StoreError::ManagedRunActive { .. }) => {
            return Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!(
                        "{pages_copied} page(s) were copied to {}/{}, but the source \
                         {label} could not be purged: {e}. The source is intact. \
                         move-project --force cannot delete a live managed \
                         workstream during a copy-purge merge; finish or cancel \
                         the managed run, then retry.",
                        req.to_workspace, req.project,
                    )
                })),
            ));
        }
        Err(e) => return Err(internal_err(e.to_string())),
    };

    // Remove the source's wiki and raw-workstream directories, then dispatch
    // the non-blocking purge webhook. Pass the workspace/project names cached
    // in `resolved_purge_ctx`; the DB rows no longer exist for lookup.
    let (files_deleted, files_failed) = remove_purged_project_storage(
        state,
        src_ws,
        src_proj,
        &summary.workstream_ids,
        "move-project",
    )
    .await;
    // See `handle_purge_project` for the rationale on `partial_failure`.
    let mut dispatch_ctx = resolved_purge_ctx;
    if !files_failed.is_empty()
        && let Some(ref mut c) = dispatch_ctx
    {
        c.partial_failure = true;
    }
    state.wiki.dispatch_purge_project(dispatch_ctx.as_ref());

    // The source project_id was just purged; if it was the published active
    // project, the pointer now dangles — clear it so the next hook re-resolves
    // to the (new) project rather than the deleted id.
    state.active_project.clear_project(src_proj);
    // Proactively drop any stale per-cwd cache entry for the purged source
    // project (its project_id no longer exists).
    invalidate_scope_cache(state, ScopeInvalidation::Project(src_proj)).await;

    if let Err(e) = state.wiki.backfill_scope_manifests().await {
        warn!(error = %e, "copy-purge move: scope-manifest backfill failed after move");
    }
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!(
            "move-project {}/{} -> {}/{}",
            req.from_workspace, req.project, req.to_workspace, req.project
        ),
    );

    let report = MoveProjectReport {
        from: label,
        to: format!("{}/{}", req.to_workspace, req.project),
        // copy_purge_merge is only reached from the merge branch.
        merged_into_existing: true,
        moved_via: "copy-purge",
        pages_copied,
        workstreams_moved: 0,
        pages_skipped: Vec::new(),
        source_purged: true,
        source_pages_deleted: summary.pages_deleted,
        source_sessions_deleted: summary.sessions_deleted,
        source_observations_deleted: summary.observations_deleted,
        source_handoffs_deleted: summary.handoffs_deleted,
        source_embeddings_deleted: summary.embeddings_deleted,
        files_deleted,
        files_failed,
        conflicts,
        pre_checkpoint,
        checkpoint,
    };
    Ok(report)
}

// ---------------------------------------------------------------------
// merge-workspace
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/merge-workspace`.
#[derive(Deserialize)]
struct MergeWorkspaceRequest {
    /// Source workspace to drain and delete. Must exist; 404 on a typo.
    from: String,
    /// Destination workspace. Auto-created if absent (each per-project move
    /// get-or-creates it).
    to: String,
    /// Mandatory confirmation. The merge moves every source project (each a
    /// destructive move that purges the source project) and deletes the emptied
    /// source workspace, so without `confirm: true` the server returns 400.
    /// Defaulted so an omitted flag hits that explicit guard rather than a
    /// generic deserialization 422.
    #[serde(default)]
    confirm: bool,
    /// Override the live-session guard on every per-project move.
    #[serde(default)]
    force: bool,
    /// Conflict policy for a source project that already exists in the
    /// destination with colliding page paths (copy-purge merge). Default
    /// `block`. Projects with no destination twin are lossless true-moves and
    /// ignore this.
    #[serde(default)]
    on_conflict: OnConflict,
}

/// Wire-format report returned by `POST /admin/merge-workspace`.
#[derive(Debug, Serialize)]
pub struct MergeWorkspaceReport {
    /// Source workspace label.
    pub from: String,
    /// Destination workspace label.
    pub to: String,
    /// One move report per source project, in the order they were moved.
    pub projects_merged: Vec<MoveProjectReport>,
    /// Whether the now-empty source workspace was deleted at the end.
    pub source_workspace_deleted: bool,
}

/// `POST /admin/merge-workspace` — fold every project of `from` into `to`, then
/// delete the emptied `from`. Sugar over move-project: each source project runs
/// the same validated path, so a project that already exists in `to` merges by
/// content (copy-purge under `on_conflict`) while a fresh one is a lossless
/// re-stamp. Destructive (each move purges its source project and the source
/// workspace is removed), so `confirm=true` is required. Stops at the first
/// failing project — the moves already committed stand, and the failing project
/// plus the source workspace are left intact for the operator to resolve and
/// re-run (moves are idempotent).
async fn handle_merge_workspace(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    Json(req): Json<MergeWorkspaceRequest>,
) -> impl IntoResponse {
    if !req.confirm {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "destructive operation requires confirm=true"
            })),
        );
    }
    if req.from == req.to {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "from and to are identical; nothing to merge"
            })),
        );
    }

    // Resolve the SOURCE without auto-creating — 404 on a typo, so a mistyped
    // source can't create then immediately delete a phantom workspace.
    if let Err(e) = lookup_ws_no_create(&state, &req.from).await {
        return e;
    }

    let projects = match state
        .reader
        .list_projects_with_stats_for_workspace(req.from.clone())
        .await
    {
        Ok(p) => p,
        Err(e) => return internal_err(e.to_string()),
    };

    let actor = actor_ext
        .map(|axum::Extension(a)| a)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);

    let mut merged: Vec<MoveProjectReport> = Vec::with_capacity(projects.len());
    for p in &projects {
        let move_req = MoveProjectRequest {
            from_workspace: req.from.clone(),
            project: p.project_name.clone(),
            to_workspace: req.to.clone(),
            confirm: true,
            force: req.force,
            on_conflict: req.on_conflict,
        };
        match move_project_core(&state, &move_req, actor.clone()).await {
            Ok(report) => merged.push(report),
            Err((status, body)) => {
                let reason = body
                    .0
                    .get("error")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                return (
                    status,
                    Json(serde_json::json!({
                        "error": format!(
                            "merge stopped at project '{}' in workspace '{}'",
                            p.project_name, req.from
                        ),
                        "failed_project": p.project_name,
                        "reason": reason,
                        "projects_merged": merged,
                        "source_workspace_deleted": false,
                    })),
                );
            }
        }
    }

    // Every project moved → the source workspace is now empty. Delete the shell
    // with force=false: it must be empty, so a race that repopulated it aborts
    // safely without wiping fresh data.
    let source_workspace_deleted = match delete_workspace_core(
        &state,
        &req.from,
        false,
        actor,
        // The shell is empty by this point; there is nothing to reclaim.
        ai_memory_store::Compaction::Skip,
    )
    .await
    {
        Ok(_) => true,
        Err((_status, body)) => {
            // The moves succeeded; only the final empty-shell delete failed
            // (e.g. a concurrent write recreated a project). No data was
            // lost, so report success-with-caveat instead of a hard error.
            warn!(
                workspace = %req.from,
                "merge-workspace: source drained but shell delete failed: {:?}",
                body
            );
            false
        }
    };

    let report = MergeWorkspaceReport {
        from: req.from,
        to: req.to,
        projects_merged: merged,
        source_workspace_deleted,
    };
    (
        StatusCode::OK,
        Json(serde_json::to_value(&report).unwrap_or(serde_json::Value::Null)),
    )
}

// ---------------------------------------------------------------------
// write-page
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/write-page`.
#[derive(Deserialize)]
struct WritePageAdminRequest {
    /// Workspace name (auto-created if absent).
    workspace: String,
    /// Project name (auto-created if absent).
    project: String,
    /// Relative wiki path (e.g. `concepts/foo.md`).
    path: String,
    /// Markdown body. The server frames it with frontmatter; pass plain body content.
    body: String,
    /// Optional title; derived from first H1 or path stem when absent.
    #[serde(default)]
    title: Option<String>,
    /// Semantic kind (`fact`, `rule`, `decision`, `gotcha`). Stored in
    /// the page frontmatter; the reader falls back to a path-derived
    /// kind when absent.
    #[serde(default)]
    kind: Option<String>,
    /// Tier name (`working`, `episodic`, `semantic`, `procedural`).
    #[serde(default = "default_write_tier")]
    tier: String,
    /// Tags to attach to the page.
    #[serde(default)]
    tags: Vec<String>,
    /// Pin the page so the decay sweep skips it.
    #[serde(default)]
    pinned: bool,
}

fn default_write_tier() -> String {
    "semantic".to_string()
}

/// JSON response body for `POST /admin/write-page`.
#[derive(Serialize)]
struct WritePageResponse {
    /// UUID of the written page.
    page_id: String,
    /// Canonical wiki path.
    path: String,
    /// Post-write checkpoint, if the write changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
}

async fn handle_write_page(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    author_ext: Option<axum::Extension<ai_memory_core::UserId>>,
    level_ext: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    headers: HeaderMap,
    Json(req): Json<WritePageAdminRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let tier: Tier = req.tier.parse().map_err(|_| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": format!("unknown tier '{}'", req.tier)
            })),
        )
    })?;

    let path = PagePath::new(req.path.clone()).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": format!("invalid path: {e}") })),
        )
    })?;

    let (ws, proj) = create_ws_proj(&state, &req.workspace, &req.project).await?;

    let mut fm = serde_json::Map::new();
    if let Some(title) = &req.title {
        fm.insert("title".into(), serde_json::Value::String(title.clone()));
    }
    if let Some(kind) = req.kind.as_deref() {
        let kind = kind.trim();
        if !kind.is_empty() {
            fm.insert("kind".into(), serde_json::Value::String(kind.to_string()));
        }
    }
    if !req.tags.is_empty() {
        fm.insert(
            "tags".into(),
            serde_json::Value::Array(
                req.tags
                    .into_iter()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
    }
    if req.pinned {
        fm.insert("pinned".into(), serde_json::Value::Bool(true));
    }
    let frontmatter = if fm.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::Object(fm)
    };

    let actor = actor_ext
        .map(|axum::Extension(actor)| actor)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    let author_id = author_ext.map(|axum::Extension(author_id)| author_id);
    let skip_webhooks = skip_webhooks_for_admin_request(level_ext, &headers);
    let admission_ctx = if actor.has_any() || !skip_webhooks.is_empty() {
        Some(AdmissionContext {
            op: AdmissionOp::WritePage,
            skip_webhooks,
            ..AdmissionContext::default()
        })
    } else {
        None
    };

    let page_id = state
        .wiki
        .write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: path.clone(),
            frontmatter,
            body: req.body,
            tier,
            pinned: req.pinned,
            title: req.title,
            admission_ctx,
            author_id,
            actor,
        })
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!(
            "write-page {}/{}: {}",
            req.workspace,
            req.project,
            path.as_str()
        ),
    );

    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(WritePageResponse {
                page_id: page_id.to_string(),
                path: path.to_string(),
                checkpoint,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
    ))
}

// ---------------------------------------------------------------------
// delete-page
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/delete-page`.
///
/// Unlike `memory_delete_page` (MCP), this endpoint REQUIRES explicit
/// `workspace` so cross-workspace ambiguity can never silently route a
/// delete to the wrong slot.
#[derive(Deserialize)]
struct DeletePageAdminRequest {
    /// Workspace name. Required (no auto-create — delete acts on existing data).
    workspace: String,
    /// Project name within the workspace. Required.
    project: String,
    /// Relative wiki path (e.g. `concepts/foo.md`).
    path: String,
}

/// JSON response body for `POST /admin/delete-page`.
#[derive(Serialize)]
struct DeletePageResponse {
    /// Canonical wiki path of the deletion target.
    path: String,
    /// Always `true` on a successful (resolved-scope) call. `Wiki::delete_page`
    /// itself is idempotent — a missing file is treated as already-deleted —
    /// so the boolean reports "the call succeeded", not "a row was removed".
    /// The structural defense is in the 404 returned when `(workspace, project)`
    /// fails to resolve (so a stale or wrong-scope call never returns a misleading
    /// `deleted: true`).
    deleted: bool,
    /// Pre-delete checkpoint, if the tree had uncommitted changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pre_checkpoint: Option<String>,
    /// Post-delete checkpoint, if the delete changed the tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    checkpoint: Option<String>,
}

async fn handle_delete_page(
    State(state): State<Arc<AdminState>>,
    actor_ext: Option<axum::Extension<ai_memory_core::ActorContext>>,
    level_ext: Option<axum::Extension<ai_memory_core::AuthLevel>>,
    author_ext: Option<axum::Extension<ai_memory_core::UserId>>,
    headers: HeaderMap,
    Json(req): Json<DeletePageAdminRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let path = PagePath::new(req.path.clone()).map_err(|e| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({ "error": format!("invalid path: {e}") })),
        )
    })?;

    // Use the no-create lookup (same as purge/rename/move): a delete on a
    // typo'd workspace/project must return 404, NOT silently auto-create
    // empty containers and then return `deleted: true` for nothing.
    let (ws, proj) = lookup_ws_proj_no_create(&state, &req.workspace, &req.project).await?;

    let pre_checkpoint = checkpoint_or_500(
        &state.wiki,
        format!(
            "pre-delete-page {}/{}: {}",
            req.workspace,
            req.project,
            path.as_str()
        ),
    )?;

    let actor = actor_ext
        .map(|axum::Extension(actor)| actor)
        .unwrap_or_else(ai_memory_core::ActorContext::anonymous);
    let skip_webhooks = skip_webhooks_for_admin_request(level_ext, &headers);
    let admission_ctx = if actor.has_any() || !skip_webhooks.is_empty() {
        Some(AdmissionContext {
            actor,
            op: AdmissionOp::Delete,
            skip_webhooks,
            ..AdmissionContext::default()
        })
    } else {
        None
    };

    let author_id = author_ext.map(|axum::Extension(u)| u);
    state
        .wiki
        .delete_page(ws, proj, &path, admission_ctx, author_id)
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let checkpoint = checkpoint_or_warn(
        &state.wiki,
        format!(
            "delete-page {}/{}: {}",
            req.workspace,
            req.project,
            path.as_str()
        ),
    );

    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(DeletePageResponse {
                path: path.to_string(),
                deleted: true,
                pre_checkpoint,
                checkpoint,
            })
            .unwrap_or_else(|_| serde_json::json!({})),
        ),
    ))
}

// ---------------------------------------------------------------------
// user management (root-only)
// ---------------------------------------------------------------------

/// JSON request body for `POST /admin/users`.
#[derive(Debug, Deserialize)]
struct CreateUserRequest {
    username: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    role: Option<ai_memory_core::UserRole>,
}

#[derive(Debug, Deserialize)]
struct PatchUserRequest {
    #[serde(default)]
    name: Option<Option<String>>,
    #[serde(default)]
    email: Option<Option<String>>,
    #[serde(default)]
    role: Option<ai_memory_core::UserRole>,
}

#[derive(Debug, Deserialize)]
struct CreateApiCredentialRequest {
    username: String,
    label: String,
}

#[derive(Debug, Serialize)]
struct UserWithTokenResponse {
    user: ai_memory_core::User,
    token: String,
}

#[derive(Debug, Serialize)]
struct UserWithPasswordResponse {
    user: ai_memory_core::User,
    temporary_password: String,
}

#[derive(Debug, Serialize)]
struct UserResponse {
    user: ai_memory_core::User,
}

#[derive(Debug, Serialize)]
struct UserListResponse {
    users: Vec<ai_memory_core::User>,
}

#[derive(Debug, Serialize)]
struct ApiCredentialWithTokenResponse {
    credential: ai_memory_core::ApiCredential,
    token: String,
}

#[derive(Debug, Serialize)]
struct ApiCredentialListResponse {
    credentials: Vec<ai_memory_core::ApiCredential>,
}

fn require_root(
    level: ai_memory_core::AuthLevel,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    level
        .authorize(ai_memory_core::Capability::UserManagement, true)
        .map_err(|e| {
            let status = if e.is_authentication_required() {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::FORBIDDEN
            };
            (status, Json(serde_json::json!({ "error": e.message() })))
        })
}

fn require_pepper(
    state: &AdminState,
) -> Result<&ai_memory_store::TokenPepper, (StatusCode, Json<serde_json::Value>)> {
    state.token_pepper.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "multi-user not enabled (set [auth].token_pepper in config or run `ai-memory init`)"
            })),
        )
    })
}

fn reserved_password(
    auth: Option<axum::Extension<std::sync::Arc<crate::auth::AuthState>>>,
    password: &str,
) -> bool {
    auth.is_some_and(|axum::Extension(state)| {
        crate::human_auth::password_is_reserved(&state, password)
    })
}

async fn hash_temp_password() -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let raw = ai_memory_store::password::generate_temporary_password()
        .map_err(|e| internal_err(e.to_string()))?;
    ai_memory_core::validate_human_password(&raw, None, &[])
        .map_err(|e| validation_error(e.to_string()))?;
    Ok(raw)
}

async fn mint_temporary_password(
    state: &AdminState,
    auth: Option<axum::Extension<Arc<crate::auth::AuthState>>>,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    for _ in 0..8 {
        let raw = hash_temp_password().await?;
        if reserved_password(auth.clone(), &raw) {
            continue;
        }
        if let Some(pepper) = state.token_pepper.as_ref() {
            let hash = ai_memory_store::hash_token(&raw, pepper);
            match state.reader.token_hash_exists(hash).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => return Err(internal_err(e.to_string())),
            }
        }
        return Ok(raw);
    }
    Err(internal_err(
        "could not mint a unique temporary password".to_string(),
    ))
}

async fn create_human_user_response(
    state: &AdminState,
    auth: Option<axum::Extension<Arc<crate::auth::AuthState>>>,
    req: CreateUserRequest,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    let mut new_user = ai_memory_core::NewUser {
        username: req.username,
        name: req.name,
        email: req.email,
    };
    new_user
        .validate()
        .map_err(|e| validation_error(e.to_string()))?;
    let role = req.role.unwrap_or(ai_memory_core::UserRole::User);
    let raw = mint_temporary_password(state, auth).await?;
    let phc = ai_memory_store::password::hash_password(raw.clone())
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let user_id = state
        .writer
        .create_human_user(new_user, role, Some(phc), true)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user_id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("created user vanished from store".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(UserWithPasswordResponse {
                user,
                temporary_password: raw,
            })
            .unwrap_or_default(),
        ),
    ))
}

async fn handle_create_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    auth: Option<axum::Extension<Arc<crate::auth::AuthState>>>,
    Json(req): Json<CreateUserRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    if req.role.is_some() {
        return create_human_user_response(&state, auth, req).await;
    }
    let pepper = require_pepper(&state)?;
    let mut new_user = ai_memory_core::NewUser {
        username: req.username,
        name: req.name,
        email: req.email,
    };
    new_user
        .validate()
        .map_err(|e| validation_error(e.to_string()))?;
    let token = ai_memory_store::generate_token().map_err(|e| internal_err(e.to_string()))?;
    let token_hash = ai_memory_store::hash_token(&token, pepper);
    let user_id = state
        .writer
        .create_user(new_user, token_hash)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user_id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("created user vanished from store".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserWithTokenResponse { user, token }).unwrap_or_default()),
    ))
}

async fn handle_create_human_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    auth: Option<axum::Extension<Arc<crate::auth::AuthState>>>,
    Json(req): Json<CreateUserRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    create_human_user_response(&state, auth, req).await
}

async fn handle_list_users(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let users = state
        .reader
        .list_users()
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserListResponse { users }).unwrap_or_default()),
    ))
}

async fn handle_expire_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    state
        .writer
        .expire_user_token(user.id)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after token expiry".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserResponse { user }).unwrap_or_default()),
    ))
}

async fn handle_revive_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    state
        .writer
        .revive_user_token(user.id)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after token revival".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserResponse { user }).unwrap_or_default()),
    ))
}

async fn handle_rotate_user_token(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let pepper = require_pepper(&state)?;
    let user = lookup_user_by_username(&state, &username).await?;
    let token = ai_memory_store::generate_token().map_err(|e| internal_err(e.to_string()))?;
    let token_hash = ai_memory_store::hash_token(&token, pepper);
    let updated = state
        .writer
        .rotate_user_token(user.id, token_hash)
        .await
        .map_err(map_user_store_err)?;
    if !updated {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "user vanished mid-rotation" })),
        ));
    }
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after token rotation".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserWithTokenResponse { user, token }).unwrap_or_default()),
    ))
}

async fn handle_patch_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
    Json(req): Json<PatchUserRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    state
        .writer
        .patch_user(user.id, req.name, req.email, req.role)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after patch".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserResponse { user }).unwrap_or_default()),
    ))
}

async fn handle_reset_password(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    auth: Option<axum::Extension<Arc<crate::auth::AuthState>>>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    let raw = mint_temporary_password(&state, auth).await?;
    let phc = ai_memory_store::password::hash_password(raw.clone())
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    state
        .writer
        .reset_human_password(user.id, phc, true)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after reset".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(UserWithPasswordResponse {
                user,
                temporary_password: raw,
            })
            .unwrap_or_default(),
        ),
    ))
}

async fn handle_disable_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    state
        .writer
        .set_user_disabled(user.id, true)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after disable".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserResponse { user }).unwrap_or_default()),
    ))
}

async fn handle_enable_user(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    axum::extract::Path(username): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let user = lookup_user_by_username(&state, &username).await?;
    state
        .writer
        .set_user_disabled(user.id, false)
        .await
        .map_err(map_user_store_err)?;
    let user = state
        .reader
        .find_user_by_id(user.id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("user vanished after enable".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(UserResponse { user }).unwrap_or_default()),
    ))
}

async fn handle_create_api_credential(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    Json(req): Json<CreateApiCredentialRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let pepper = require_pepper(&state)?;
    let user = lookup_user_by_username(&state, &req.username).await?;
    let token = ai_memory_store::generate_api_key().map_err(|e| internal_err(e.to_string()))?;
    let token_hash = ai_memory_store::hash_token(&token, pepper);
    let preview = ai_memory_store::api_key_preview(&token);
    let id = state
        .writer
        .create_api_credential(
            ai_memory_core::ApiCredentialId::new(),
            user.id,
            req.label,
            token_hash,
            Some(preview),
        )
        .await
        .map_err(map_user_store_err)?;
    let credential = state
        .reader
        .find_api_credential(id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("credential vanished after create".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(ApiCredentialWithTokenResponse { credential, token })
                .unwrap_or_default(),
        ),
    ))
}

async fn handle_list_api_credentials(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let credentials = state
        .reader
        .list_api_credentials()
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(ApiCredentialListResponse { credentials }).unwrap_or_default()),
    ))
}

async fn handle_rotate_api_credential(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let pepper = require_pepper(&state)?;
    let id: ai_memory_core::ApiCredentialId = id
        .parse()
        .map_err(|e: ai_memory_core::MemoryError| validation_error(e.to_string()))?;
    let token = ai_memory_store::generate_api_key().map_err(|e| internal_err(e.to_string()))?;
    let token_hash = ai_memory_store::hash_token(&token, pepper);
    let preview = ai_memory_store::api_key_preview(&token);
    let updated = state
        .writer
        .rotate_api_credential(id, token_hash, Some(preview))
        .await
        .map_err(map_user_store_err)?;
    if !updated {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "no such credential" })),
        ));
    }
    let credential = state
        .reader
        .find_api_credential(id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| internal_err("credential vanished after rotate".to_string()))?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::to_value(ApiCredentialWithTokenResponse { credential, token })
                .unwrap_or_default(),
        ),
    ))
}

async fn handle_revoke_api_credential(
    State(state): State<Arc<AdminState>>,
    axum::Extension(level): axum::Extension<ai_memory_core::AuthLevel>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    require_root(level)?;
    let id: ai_memory_core::ApiCredentialId = id
        .parse()
        .map_err(|e: ai_memory_core::MemoryError| validation_error(e.to_string()))?;
    state
        .writer
        .revoke_api_credential(id)
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    let credential = state
        .reader
        .find_api_credential(id)
        .await
        .map_err(|e| internal_err(e.to_string()))?
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "no such credential" })),
            )
        })?;
    Ok((
        StatusCode::OK,
        Json(serde_json::json!({ "credential": credential })),
    ))
}

async fn lookup_user_by_username(
    state: &AdminState,
    username: &str,
) -> Result<ai_memory_core::User, (StatusCode, Json<serde_json::Value>)> {
    let found = state
        .reader
        .find_user_by_username(username.to_string())
        .await
        .map_err(|e| internal_err(e.to_string()))?;
    found.ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("no such user: {username}") })),
        )
    })
}

fn validation_error(msg: String) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
}

fn map_user_store_err(e: ai_memory_store::StoreError) -> (StatusCode, Json<serde_json::Value>) {
    match e {
        ai_memory_store::StoreError::Duplicate(msg)
        | ai_memory_store::StoreError::InvalidState(msg) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": msg })),
        ),
        ai_memory_store::StoreError::Memory(ai_memory_core::MemoryError::InvalidUsername(msg))
        | ai_memory_store::StoreError::Memory(ai_memory_core::MemoryError::InvalidEmail(msg))
        | ai_memory_store::StoreError::Memory(ai_memory_core::MemoryError::InvalidPassword(msg)) => {
            validation_error(msg)
        }
        other => internal_err(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{
        ActorContext, AgentKind, IdentityKey, NewObservation, NewSession, ObservationKind,
    };
    use ai_memory_core::{Sanitized, Sanitizer};
    use ai_memory_llm::{ChatRequest, ChatResponse, LlmResult};
    use ai_memory_store::Store;
    use axum::body::to_bytes;
    use axum::http::Request;
    use tempfile::TempDir;
    use tower::ServiceExt;

    struct FakeAutoImproveLlm;

    #[async_trait::async_trait]
    impl LlmProvider for FakeAutoImproveLlm {
        fn name(&self) -> &'static str {
            "fake-auto-improve"
        }

        fn model(&self) -> &str {
            "fake-model"
        }

        async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                text: String::new(),
                usage: None,
                model: self.model().to_string(),
            })
        }

        async fn complete_structured_raw(
            &self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> LlmResult<serde_json::Value> {
            Ok(serde_json::json!({
                "summary": "two staged proposals",
                "proposals": [
                    {
                        "operation": "create_or_update",
                        "path": "_slots/current-focus.md",
                        "title": "Current Focus",
                        "kind": "slot",
                        "confidence": 0.95,
                        "rationale": "The current focus should be updated from the session.",
                        "evidence": [{"page":"session", "quote":"focus changed"}],
                        "body_markdown": "# Current Focus\n\nupdated focus proposal"
                    },
                    {
                        "operation": "create_or_update",
                        "path": "notes/new-auto-improve.md",
                        "title": "New Auto Improve Lesson",
                        "kind": "note",
                        "confidence": 0.93,
                        "rationale": "The session contains a durable lesson worth adding.",
                        "evidence": [{"page":"session", "quote":"durable lesson"}],
                        "body_markdown": "# New Auto Improve Lesson\n\nnew page proposal"
                    }
                ],
                "rejected_candidates": []
            }))
        }
    }

    #[tokio::test]
    async fn status_reports_provider_health_block() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["providers"]["llm"]["status"], "disabled");
        assert_eq!(json["providers"]["embedding"]["status"], "disabled");
    }

    /// The MCP-only complement: per-client tool-call counters served
    /// back with the same window semantics as by-agent.
    #[tokio::test]
    async fn activity_by_client_aggregates_and_bounds_the_window() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let today = jiff::Timestamp::now()
            .as_microsecond()
            .div_euclid(US_PER_DAY);
        store
            .writer
            .bump_client_activity(vec![
                ("vscode".into(), today, 4, 1),
                ("vscode".into(), today - 40, 7, 0),
                ("claude-desktop".into(), today, 1, 0),
            ])
            .await
            .unwrap();

        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49376".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });

        // Whole history: the 40-day-old bucket counts.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/activity/by-client")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["by_client"],
            serde_json::json!([
                { "client": "vscode", "reads": 11, "writes": 1 },
                { "client": "claude-desktop", "reads": 1, "writes": 0 },
            ]),
            "volume-desc across all history: {json}"
        );

        // A 7-day window drops the old bucket.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/activity/by-client?since_days=7")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["by_client"][0],
            serde_json::json!({ "client": "vscode", "reads": 4, "writes": 1 }),
            "{json}"
        );
    }

    /// The dashboard read: session counts grouped per agent CLI, with the
    /// scope failing closed and never auto-created.
    #[tokio::test]
    async fn sessions_by_agent_counts_per_agent_and_fails_closed_on_unknown_scope() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store
            .writer
            .get_or_create_workspace("default".to_string())
            .await
            .unwrap();
        let target = store
            .writer
            .get_or_create_project(ws, "target".to_string(), None)
            .await
            .unwrap();
        let other_project = store
            .writer
            .get_or_create_project(ws, "other".to_string(), None)
            .await
            .unwrap();
        for (project_id, agent) in [
            (target, AgentKind::ClaudeCode),
            (target, AgentKind::ClaudeCode),
            (target, AgentKind::Cursor),
            // A different scope must not leak into the target's totals.
            (other_project, AgentKind::Codex),
        ] {
            store
                .writer
                .begin_session(NewSession {
                    id: SessionId::new(),
                    workspace_id: ws,
                    project_id,
                    agent_kind: agent,
                    cwd: None,
                    actor_user: None,
                })
                .await
                .unwrap();
        }
        for (agent, owner) in [(AgentKind::ClaudeCode, "alice"), (AgentKind::Codex, "bob")] {
            store
                .writer
                .begin_session(NewSession {
                    id: SessionId::new(),
                    workspace_id: ws,
                    project_id: target,
                    agent_kind: agent,
                    cwd: None,
                    actor_user: Some(IdentityKey::User(owner.into()).storage_key()),
                })
                .await
                .unwrap();
        }

        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49375".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/sessions/by-agent?workspace=default&project=target")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["by_agent"],
            serde_json::json!([
                { "agent": "claude-code", "sessions": 2 },
                { "agent": "cursor", "sessions": 1 },
            ]),
            "counts are per agent, scoped, count-desc: {json}"
        );

        // A named operator sees their rows plus shared legacy rows, but not a
        // colleague's. The recovery switch deliberately includes all owners.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/sessions/by-agent?workspace=default&project=target")
                    .extension(ActorContext {
                        user: Some("alice".into()),
                        ..ActorContext::default()
                    })
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["by_agent"],
            serde_json::json!([
                { "agent": "claude-code", "sessions": 3 },
                { "agent": "cursor", "sessions": 1 },
            ]),
            "named callers see own plus shared sessions only: {json}"
        );

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(
                        "/admin/sessions/by-agent?workspace=default&project=target&all_owners=true",
                    )
                    .extension(ActorContext {
                        user: Some("alice".into()),
                        ..ActorContext::default()
                    })
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["by_agent"],
            serde_json::json!([
                { "agent": "claude-code", "sessions": 3 },
                { "agent": "codex", "sessions": 1 },
                { "agent": "cursor", "sessions": 1 },
            ]),
            "all_owners includes every operator: {json}"
        );

        // Zero is the explicit all-history spelling and must not become an
        // empty time window.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/sessions/by-agent?workspace=default&project=target&since_days=0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["by_agent"].as_array().unwrap().len(),
            2,
            "since_days=0 means the whole history, not an empty window: {json}"
        );

        // A window wider than the epoch saturates into "all history" rather
        // than overflowing: `u32::MAX` days times a day of microseconds does
        // not fit in the i64 the cutoff is computed in.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(
                        "/admin/sessions/by-agent\
                         ?workspace=default&project=target&since_days=4294967295",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["by_agent"].as_array().unwrap().len(),
            2,
            "a saturating window counts everything, it does not panic or empty: {json}"
        );

        // Unknown scope fails closed with a 404 — never auto-created.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/sessions/by-agent?workspace=default&project=ghost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(
            store
                .reader
                .find_project(ws, "ghost".to_string())
                .await
                .unwrap()
                .is_none(),
            "read route must not auto-create scopes"
        );
    }

    #[tokio::test]
    async fn open_sessions_filters_by_scope_and_agent() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store
            .writer
            .get_or_create_workspace("default".to_string())
            .await
            .unwrap();
        let target = store
            .writer
            .get_or_create_project(ws, "target".to_string(), None)
            .await
            .unwrap();
        let other_project = store
            .writer
            .get_or_create_project(ws, "other".to_string(), None)
            .await
            .unwrap();
        let older = SessionId::new();
        let latest = SessionId::new();
        let other_agent = SessionId::new();
        let other_scope = SessionId::new();
        let ended = SessionId::new();
        let alice = SessionId::new();
        let bob = SessionId::new();
        for (id, project_id, agent) in [
            (older, target, AgentKind::Codex),
            (other_agent, target, AgentKind::ClaudeCode),
            (other_scope, other_project, AgentKind::Codex),
            (ended, target, AgentKind::Codex),
            (latest, target, AgentKind::Codex),
        ] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id,
                    agent_kind: agent,
                    cwd: Some(std::path::PathBuf::from("/tmp/target")),
                    actor_user: None,
                })
                .await
                .unwrap();
        }
        store.writer.end_session(ended, None).await.unwrap();
        for (id, user) in [(alice, "alice"), (bob, "bob")] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id: target,
                    agent_kind: AgentKind::Codex,
                    cwd: Some(std::path::PathBuf::from("/tmp/target")),
                    actor_user: Some(IdentityKey::User(user.into()).storage_key()),
                })
                .await
                .unwrap();
        }

        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });

        // Default: only the newest open session for the scope + agent.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/open-sessions?workspace=default&project=target&agent=codex")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let sessions = json["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"], latest.to_string());
        assert_eq!(sessions[0]["cwd"], "/tmp/target");

        // `all=true`: every open codex session in the scope, ended and
        // other-scope/other-agent sessions excluded.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/open-sessions?workspace=default&project=target&agent=codex&all=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let sessions = json["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        let ids: Vec<&str> = sessions
            .iter()
            .map(|s| s["session_id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&latest.to_string().as_str()));
        assert!(ids.contains(&older.to_string().as_str()));

        // A named operator gets their own open sessions plus shared legacy
        // sessions, never a colleague's. This is the exact route consumed by
        // the destructive `finalize-session` command.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/open-sessions?workspace=default&project=target&agent=codex&all=true")
                    .extension(ActorContext {
                        user: Some("alice".into()),
                        ..ActorContext::default()
                    })
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ids = json["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["session_id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(ids.contains(&alice.to_string().as_str()));
        assert!(ids.contains(&latest.to_string().as_str()));
        assert!(ids.contains(&older.to_string().as_str()));
        assert!(!ids.contains(&bob.to_string().as_str()));

        // Unknown agent kind fails closed with a 400.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/open-sessions?workspace=default&project=target&agent=nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Unknown scope fails closed with a 404 — never auto-created.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/open-sessions?workspace=default&project=ghost&agent=codex")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(
            store
                .reader
                .find_project(ws, "ghost".to_string())
                .await
                .unwrap()
                .is_none(),
            "read route must not auto-create scopes"
        );
    }

    /// Regression for the race a naive wrapper hits with several concurrent
    /// Kiro CLI tabs against one repo: two open sessions for the same
    /// agent+scope exist (`older`, `latest`), and `?session_id=` must return
    /// exactly the one requested — never falling back to "newest open" —
    /// while still composing with the owner filter (a session_id belonging
    /// to another operator must not be reachable just by knowing its id,
    /// same invariant `all_owners` already protects for the default query).
    #[tokio::test]
    async fn open_sessions_session_id_targets_exact_session() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store
            .writer
            .get_or_create_workspace("default".to_string())
            .await
            .unwrap();
        let target = store
            .writer
            .get_or_create_project(ws, "target".to_string(), None)
            .await
            .unwrap();
        let other_project = store
            .writer
            .get_or_create_project(ws, "other".to_string(), None)
            .await
            .unwrap();
        let older = SessionId::new();
        let latest = SessionId::new();
        let other_scope = SessionId::new();
        let ended = SessionId::new();
        let bobs_session = SessionId::new();
        let wrong_agent = SessionId::new();
        for (id, project_id, agent_kind, actor_user) in [
            (older, target, AgentKind::KiroCli, None),
            (latest, target, AgentKind::KiroCli, None),
            (other_scope, other_project, AgentKind::KiroCli, None),
            (ended, target, AgentKind::KiroCli, None),
            (wrong_agent, target, AgentKind::Codex, None),
            (
                bobs_session,
                target,
                AgentKind::KiroCli,
                Some(IdentityKey::User("bob".into()).storage_key()),
            ),
        ] {
            store
                .writer
                .begin_session(NewSession {
                    id,
                    workspace_id: ws,
                    project_id,
                    agent_kind,
                    cwd: Some(std::path::PathBuf::from("/tmp/target")),
                    actor_user,
                })
                .await
                .unwrap();
        }
        store.writer.end_session(ended, None).await.unwrap();

        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });

        // Targeting `older` by id must return only `older`, not `latest`.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={older}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let sessions = json["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "must not fall back to newest-open");
        assert_eq!(sessions[0]["session_id"], older.to_string());

        // A session id that belongs to a different project must not be
        // reachable just by knowing the id — scope is still enforced.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={other_scope}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["sessions"].as_array().unwrap().len(),
            0,
            "a session from another project must not be reachable by id"
        );

        // An already-ended session id is not "open" and must not match.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={ended}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["sessions"].as_array().unwrap().len(), 0);

        // An exact id from another agent remains outside the requested
        // agent boundary even when its scope matches.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={wrong_agent}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["sessions"].as_array().unwrap().len(), 0);

        // A colleague's session id is not reachable without all_owners, even
        // when the exact id is known — session_id composes with the owner
        // filter (AND), it doesn't bypass it.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={bobs_session}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["sessions"].as_array().unwrap().len(),
            0,
            "session_id must not bypass the owner filter"
        );

        // ...but composes correctly WITH all_owners set.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id={bobs_session}&all_owners=true"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["sessions"].as_array().unwrap().len(), 1);

        // Selection modes are mutually exclusive instead of silently giving
        // one of them precedence.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&all=true&session_id={older}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // A malformed session_id fails closed with a 400, not a silent
        // empty/ignored filter.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/open-sessions?workspace=default&project=target&agent=kiro-cli&session_id=not-a-uuid")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    fn read_page_test_router() -> (TempDir, Router) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });
        (tmp, router)
    }

    fn admin_state_for_store(tmp: &TempDir, store: &Store, wiki: Wiki) -> AdminState {
        admin_state_for_store_with_llm(tmp, store, wiki, None)
    }

    fn admin_state_for_store_with_llm(
        tmp: &TempDir,
        store: &Store,
        wiki: Wiki,
        llm: Option<Arc<dyn LlmProvider>>,
    ) -> AdminState {
        AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        }
    }

    async fn stage_pending_write(
        store: &Store,
        workspace: &str,
        project: &str,
        path: &str,
        body: &str,
    ) -> (WorkspaceId, ProjectId, AutoImproveProposalId) {
        let ws = store
            .writer
            .get_or_create_workspace(workspace)
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, project, None)
            .await
            .unwrap();
        let staged = store
            .writer
            .stage_auto_improve_run(StageAutoImproveRun {
                workspace_id: ws,
                project_id: proj,
                session_id: None,
                provider: Some("test".into()),
                model: Some("model".into()),
                summary: Some("summary".into()),
                warnings_json: serde_json::json!([]),
                rejected_candidates_json: serde_json::json!([]),
                config_json: serde_json::json!({"mode":"stage"}),
                proposal_actor: ai_memory_core::ActorContext {
                    agent: Some("auto_improve".into()),
                    ..ai_memory_core::ActorContext::default()
                },
                proposals: vec![NewAutoImproveProposal {
                    operation: AutoImproveProposalOperation::Create,
                    target_path: PagePath::new(path).unwrap(),
                    kind: "note".into(),
                    title: "Pending title".into(),
                    confidence: 0.9,
                    rationale: "rationale".into(),
                    evidence_json: serde_json::json!([{"source":"test"}]),
                    body_markdown: body.into(),
                    artifact_sha256: None,
                    edit_mode: None,
                    patch_json: None,
                    expected_base_body_sha256: None,
                }],
            })
            .await
            .unwrap();
        (ws, proj, staged.proposal_ids[0])
    }

    async fn post_write_page(router: &Router, ws: &str, project: &str, path: &str, body: &str) {
        post_write_page_with_actor(router, ws, project, path, body, None).await;
    }

    async fn post_write_page_with_actor(
        router: &Router,
        ws: &str,
        project: &str,
        path: &str,
        body: &str,
        actor: Option<&'static str>,
    ) {
        let req_body = serde_json::json!({
            "workspace": ws,
            "project": project,
            "path": path,
            "body": body,
            "title": "Read-back fixture",
        });
        let mut builder = Request::builder()
            .method("POST")
            .uri("/admin/write-page")
            .header("content-type", "application/json");
        if let Some(user) = actor {
            builder = builder.extension(ai_memory_core::ActorContext {
                user: Some(user.into()),
                ..ai_memory_core::ActorContext::default()
            });
        }
        let resp = router
            .clone()
            .oneshot(
                builder
                    .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "write-page setup failed");
    }

    #[tokio::test]
    async fn read_page_path_mode_returns_full_body() {
        let (_tmp, router) = read_page_test_router();
        post_write_page(&router, "default", "audit", "notes/foo.md", "hello body").await;

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/read-page?workspace=default&project=audit&path=notes/foo.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["path"], "notes/foo.md");
        assert_eq!(json["title"], "Read-back fixture");
        assert!(
            json["body"]
                .as_str()
                .unwrap_or_default()
                .contains("hello body"),
            "body must round-trip; got {:?}",
            json["body"]
        );
        assert_eq!(json["workspace"], "default");
        assert_eq!(json["project"], "audit");
    }

    #[tokio::test]
    async fn auto_improve_requires_llm_provider() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        store
            .writer
            .get_or_create_project(ws, "audit", None)
            .await
            .unwrap();
        let router = admin_router(admin_state_for_store(&tmp, &store, wiki));

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auto-improve")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "audit",
                            "session_id": "00000000-0000-0000-0000-000000000000"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("LLM provider not configured")
        );
    }

    #[tokio::test]
    async fn auto_improve_missing_scope_does_not_fall_back_before_llm_check() {
        let (_tmp, router) = read_page_test_router();

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auto-improve")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "missing",
                            "project": "missing",
                            "session_id": "00000000-0000-0000-0000-000000000000"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn audit_contamination_partial_scope_fails_closed() {
        let (_tmp, router) = read_page_test_router();

        for uri in [
            "/admin/audit-contamination?workspace=default",
            "/admin/audit-contamination?project=ai-memory",
        ] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{uri}");
            let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert!(
                json["error"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("workspace and project must be provided together")
            );
        }
    }

    #[tokio::test]
    async fn auto_improve_auto_approves_by_default_and_uses_concrete_operations() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();

        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("_slots/current-focus.md").unwrap(),
            frontmatter: serde_json::json!({"kind":"slot"}),
            body: "# Current Focus\n\nold focus".into(),
            tier: Tier::Working,
            pinned: false,
            title: Some("Current Focus".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Other,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: "focus changed and durable lesson".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();

        let router = admin_router(admin_state_for_store_with_llm(
            &tmp,
            &store,
            wiki,
            Some(Arc::new(FakeAutoImproveLlm)),
        ));
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auto-improve")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "scratch",
                            "session_id": session_id.to_string(),
                            "min_observations": 1,
                            "min_session_duration_secs": 0,
                            "min_confidence": 0.75,
                            "max_proposals_per_run": 5
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["approval_required"], false);
        assert_eq!(json["approval_policy"], "auto_approve");
        let proposals = json["proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 2);
        for proposal in proposals {
            assert_eq!(proposal["status"], "approved");
            assert!(proposal["page_id"].as_str().is_some());
            let path = proposal["sidecar_path"].as_str().unwrap();
            assert!(
                std::path::Path::new(path).exists(),
                "sidecar file must be written: {path}"
            );
        }

        let staged = store
            .reader
            .list_auto_improve_proposals(ws, proj, Some(AutoImproveProposalStatus::Approved), 10)
            .await
            .unwrap();
        assert_eq!(staged.len(), 2, "DB proposal rows must be approved");
        let slot = staged
            .iter()
            .find(|p| p.target_path.as_str() == "_slots/current-focus.md")
            .expect("slot proposal staged");
        let note = staged
            .iter()
            .find(|p| p.target_path.as_str() == "notes/new-auto-improve.md")
            .expect("note proposal staged");
        assert_eq!(slot.operation, AutoImproveProposalOperation::Update);
        assert_eq!(note.operation, AutoImproveProposalOperation::Create);

        let existing = store
            .reader
            .page_body_by_ids(ws, proj, "_slots/current-focus.md")
            .await
            .unwrap()
            .expect("existing target remains present");
        assert_eq!(existing.body, "# Current Focus\n\nupdated focus proposal");
        let note_page = store
            .reader
            .page_body_by_ids(ws, proj, "notes/new-auto-improve.md")
            .await
            .unwrap()
            .expect("auto-approve writes absent target page");
        assert_eq!(
            note_page.body,
            "# New Auto Improve Lesson\n\nnew page proposal"
        );
    }

    /// The V42 staging bucket is keyed on
    /// [`ai_memory_core::ActorContext::identity_key`] through
    /// [`ai_memory_core::owner_identity`], not on `actor.user`, so an operator
    /// identified by a complete OIDC issuer/subject pair lands in an
    /// issuer-qualified bucket rather than the unattributed one. The
    /// `memory_auto_improve` MCP tool computes the same bucket, and the V42
    /// one-pending-per-target promise is what breaks when it does.
    #[tokio::test]
    async fn auto_improve_buckets_an_oidc_operator_by_qualified_identity() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();

        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("_slots/current-focus.md").unwrap(),
            frontmatter: serde_json::json!({"kind":"slot"}),
            body: "# Current Focus\n\nold focus".into(),
            tier: Tier::Working,
            pinned: false,
            title: Some("Current Focus".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Other,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: "focus changed and durable lesson".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();

        // A trusted proxy is configured, so the deployment distinguishes its
        // operators even with no `users` rows — which is exactly the case where
        // reaching for `actor.user` sees nobody.
        let mut state =
            admin_state_for_store_with_llm(&tmp, &store, wiki, Some(Arc::new(FakeAutoImproveLlm)));
        state.trusted_proxy_identity = true;
        let router = admin_router(state);

        let mut req = Request::builder()
            .method("POST")
            .uri("/admin/auto-improve")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&serde_json::json!({
                    "workspace": "default",
                    "project": "scratch",
                    "session_id": session_id.to_string(),
                    "min_observations": 1,
                    "min_session_duration_secs": 0,
                    "min_confidence": 0.75,
                    "max_proposals_per_run": 5
                }))
                .unwrap(),
            ))
            .unwrap();
        req.extensions_mut().insert(ai_memory_core::ActorContext {
            issuer: Some("https://issuer.example".into()),
            sub: Some("subject-42".into()),
            ..ai_memory_core::ActorContext::anonymous()
        });
        req.extensions_mut().insert(ai_memory_core::AuthLevel::Root);

        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The bucket the contract stamps, built through the API rather than
        // hand-written, so this test follows the encoding if it ever moves.
        let expected_bucket = ai_memory_core::IdentityKey::Subject {
            issuer: "https://issuer.example".into(),
            subject: "subject-42".into(),
        }
        .storage_key();
        let staged = store
            .reader
            .list_auto_improve_proposals(ws, proj, None, 10)
            .await
            .unwrap();
        assert!(!staged.is_empty(), "the fake LLM proposes two pages");
        for proposal in &staged {
            let detail = store
                .reader
                .auto_improve_proposal_detail_with_owner(ws, proj, proposal.id)
                .await
                .unwrap()
                .expect("staged proposal readable in its own scope");
            assert_eq!(
                detail.staged_by_actor_user.as_deref(),
                Some(expected_bucket.as_str()),
                "the subject claim is the operator, not `None`: {}",
                detail.detail.summary.target_path.as_str()
            );
            let json = serde_json::to_value(&detail).unwrap();
            assert_eq!(json["staged_by_actor_user"], expected_bucket);
            assert!(json.get("summary").is_some(), "detail fields stay flat");
            assert!(
                json.get("detail").is_none(),
                "no nested compatibility wrapper"
            );
        }
    }

    /// The admin report must name the proposals the store refused to stage.
    /// Returning only the staged ids hands the operator a successful run of
    /// N-1 proposals with no trace of the Nth — the silent drop the
    /// per-proposal skip was introduced to end.
    #[tokio::test]
    async fn auto_improve_report_names_the_proposal_a_collision_skipped() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let (ws, proj, _pending) = stage_pending_write(
            &store,
            "default",
            "scratch",
            "notes/new-auto-improve.md",
            "# New Auto Improve Lesson\n\nstaged earlier",
        )
        .await;

        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("_slots/current-focus.md").unwrap(),
            frontmatter: serde_json::json!({"kind":"slot"}),
            body: "# Current Focus\n\nold focus".into(),
            tier: Tier::Working,
            pinned: false,
            title: Some("Current Focus".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Other,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: "focus changed and durable lesson".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();

        let router = admin_router(admin_state_for_store_with_llm(
            &tmp,
            &store,
            wiki,
            Some(Arc::new(FakeAutoImproveLlm)),
        ));
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auto-improve")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "scratch",
                            "session_id": session_id.to_string(),
                            "min_observations": 1,
                            "min_session_duration_secs": 0,
                            "min_confidence": 0.75,
                            "max_proposals_per_run": 5
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["proposals"].as_array().unwrap().len(),
            1,
            "the sibling proposal still stages",
        );
        let skipped = json["skipped"].as_array().expect("skipped array");
        assert_eq!(skipped.len(), 1, "the collided proposal must be reported");
        assert_eq!(skipped[0]["target_path"], "notes/new-auto-improve.md");
        assert!(
            skipped[0]["reason"]
                .as_str()
                .is_some_and(|r| !r.trim().is_empty()),
            "a skipped proposal must say why: {}",
            skipped[0]
        );
    }

    /// `[auth].root_username` alone does not make a server multi-operator: the
    /// scheduler and the report handlers stage unattributed, so bucketing this
    /// call by its actor would leave TWO pending proposals for one page on a
    /// single-operator server — the collision V42 promises cannot happen.
    #[tokio::test]
    async fn single_operator_admin_auto_improve_shares_the_unattributed_bucket() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let (ws, proj, _pending) = stage_pending_write(
            &store,
            "default",
            "scratch",
            "notes/new-auto-improve.md",
            "# New Auto Improve Lesson\n\nstaged earlier",
        )
        .await;

        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("_slots/current-focus.md").unwrap(),
            frontmatter: serde_json::json!({"kind":"slot"}),
            body: "# Current Focus\n\nold focus".into(),
            tier: Tier::Working,
            pinned: false,
            title: Some("Current Focus".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Other,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: "focus changed and durable lesson".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();

        // The root actor `[auth].root_username` produces, as the auth
        // middleware would stamp it.
        let router = admin_router(admin_state_for_store_with_llm(
            &tmp,
            &store,
            wiki,
            Some(Arc::new(FakeAutoImproveLlm)),
        ))
        .layer(axum::middleware::from_fn(
            |mut req: Request<Body>, next: axum::middleware::Next| async move {
                req.extensions_mut().insert(ai_memory_core::ActorContext {
                    user: Some("the-operator".into()),
                    ..ai_memory_core::ActorContext::default()
                });
                next.run(req).await
            },
        ));
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auto-improve")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "scratch",
                            "session_id": session_id.to_string(),
                            "min_observations": 1,
                            "min_session_duration_secs": 0,
                            "min_confidence": 0.75,
                            "max_proposals_per_run": 5
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let skipped = json["skipped"].as_array().expect("skipped array");
        assert_eq!(
            skipped.len(),
            1,
            "a named root operator must not open a second pending bucket for the same page"
        );
        assert_eq!(skipped[0]["target_path"], "notes/new-auto-improve.md");
    }

    #[tokio::test]
    async fn auto_improve_admin_omitted_eval_inherits_server_defaults() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Other,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: "focus changed and durable lesson".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
        let mut state =
            admin_state_for_store_with_llm(&tmp, &store, wiki, Some(Arc::new(FakeAutoImproveLlm)));
        state.auto_improve_review_config.eval = ai_memory_consolidate::AutoImproveEvalConfig {
            enabled: true,
            command: String::new(),
            timeout_secs: 1,
            targets: vec!["notes".into()],
            min_delta: 0.0,
        };
        let router = admin_router(state);

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auto-improve")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "scratch",
                            "session_id": session_id.to_string(),
                            "min_observations": 1,
                            "min_session_duration_secs": 0,
                            "min_confidence": 0.75,
                            "max_proposals_per_run": 5
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["rejected_candidates_count"], 1);
        let proposals = json["proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 1);
        assert_eq!(
            store
                .reader
                .list_auto_improve_proposals(ws, proj, None, 10)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn auto_improve_require_approval_keeps_proposals_pending() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let session_id = SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Other,
                cwd: None,
                actor_user: None,
            })
            .await
            .unwrap();
        store
            .writer
            .insert_observation(Sanitized::new(
                NewObservation {
                    session_id,
                    workspace_id: ws,
                    project_id: proj,
                    kind: ObservationKind::UserPrompt,
                    extension: None,
                    source_event: None,
                    title: "prompt".into(),
                    body: "durable lesson".into(),
                    importance: 5,
                },
                &Sanitizer::builtin(),
            ))
            .await
            .unwrap();
        let mut state =
            admin_state_for_store_with_llm(&tmp, &store, wiki, Some(Arc::new(FakeAutoImproveLlm)));
        state.auto_improve_require_approval = true;
        let router = admin_router(state);

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auto-improve")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "scratch",
                            "session_id": session_id.to_string(),
                            "min_observations": 1,
                            "min_session_duration_secs": 0,
                            "min_confidence": 0.75,
                            "max_proposals_per_run": 5
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["approval_required"], true);
        assert_eq!(json["approval_policy"], "manual");
        let proposals = json["proposals"].as_array().unwrap();
        assert_eq!(proposals.len(), 2);
        assert!(proposals.iter().all(|p| p["status"] == "pending"));

        let pending = store
            .reader
            .list_auto_improve_proposals(ws, proj, Some(AutoImproveProposalStatus::Pending), 10)
            .await
            .unwrap();
        assert_eq!(pending.len(), 2);
        assert!(
            store
                .reader
                .page_body_by_ids(ws, proj, "notes/new-auto-improve.md")
                .await
                .unwrap()
                .is_none(),
            "manual approval mode must not write target pages"
        );
    }

    #[tokio::test]
    async fn auto_improve_removed_mode_fields_fail_closed() {
        let (_tmp, router) = read_page_test_router();
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/auto-improve")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "scratch",
                            "session_id": "00000000-0000-0000-0000-000000000000",
                            "dry_run": true
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or_default()
                .contains("removed")
        );
    }

    #[tokio::test]
    async fn curator_dry_run_returns_report_and_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let router = admin_router(admin_state_for_store(&tmp, &store, wiki));

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/curator")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "scratch"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["workspace"], "default");
        assert_eq!(json["project"], "scratch");
        assert_eq!(json["dry_run"], true);
        assert!(json["findings"].as_array().unwrap().is_empty());
        assert!(
            store
                .reader
                .list_auto_improve_proposals(ws, proj, None, 10)
                .await
                .unwrap()
                .is_empty(),
            "dry-run must not stage proposals"
        );
        assert!(
            store
                .reader
                .list_pages("default", "scratch")
                .await
                .unwrap()
                .is_empty(),
            "dry-run must not write pages"
        );
    }

    #[tokio::test]
    async fn curator_stage_creates_one_report_proposal_and_approval_writes_report_only() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();
        let router = admin_router(admin_state_for_store(&tmp, &store, wiki));

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/curator")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "scratch",
                            "stage": true
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let proposal_ids = json["proposal_ids"].as_array().unwrap();
        assert_eq!(proposal_ids.len(), 1);
        let sidecar_paths = json["sidecar_paths"].as_array().unwrap();
        assert_eq!(sidecar_paths.len(), 1);
        let sidecar_path = sidecar_paths[0].as_str().unwrap();
        assert!(std::path::Path::new(sidecar_path).exists());

        let staged = store
            .reader
            .list_auto_improve_proposals(ws, proj, Some(AutoImproveProposalStatus::Pending), 10)
            .await
            .unwrap();
        assert_eq!(staged.len(), 1);
        let summary = &staged[0];
        assert_eq!(summary.kind, "curator_report");
        assert_eq!(summary.title, "Curator Report");
        assert_eq!(summary.operation, AutoImproveProposalOperation::Create);
        assert!(summary.target_path.as_str().starts_with("notes/curator-"));
        assert!(summary.target_path.as_str().ends_with(".md"));
        assert!(
            store
                .reader
                .page_body_by_ids(ws, proj, summary.target_path.as_str())
                .await
                .unwrap()
                .is_none(),
            "stage must not write target page"
        );

        let detail = store
            .reader
            .auto_improve_proposal_detail(ws, proj, summary.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(detail.events[0].event, "staged");
        assert_eq!(detail.events[0].actor_json["agent"], "curator");
        assert!(detail.body_markdown.starts_with("# Curator Report"));
        assert!(detail.body_markdown.contains("Report-only"));

        let approve_resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/admin/pending-writes/{}/approve?workspace=default&project=scratch",
                        summary.id
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(approve_resp.status(), StatusCode::OK);
        let page = store
            .reader
            .page_body_by_ids(ws, proj, summary.target_path.as_str())
            .await
            .unwrap()
            .expect("approval writes curator report page");
        assert!(page.body.starts_with("# Curator Report"));
        assert_eq!(
            store
                .reader
                .list_pages("default", "scratch")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn pending_writes_list_detail_diff_and_reject_use_stored_proposal() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let (_ws, _proj, proposal_id) = stage_pending_write(
            &store,
            "default",
            "scratch",
            "notes/pending.md",
            "# Pending\n\nproposed body",
        )
        .await;
        let router = admin_router(admin_state_for_store(&tmp, &store, wiki));

        let list_resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/pending-writes?workspace=default&project=scratch")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list_resp.status(), StatusCode::OK);
        let body = to_bytes(list_resp.into_body(), usize::MAX).await.unwrap();
        let list: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["id"], proposal_id.to_string());
        assert_eq!(list[0]["status"], "pending");

        let detail_resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/pending-writes/{proposal_id}?workspace=default&project=scratch"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(detail_resp.status(), StatusCode::OK);
        let body = to_bytes(detail_resp.into_body(), usize::MAX).await.unwrap();
        let detail: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(detail["summary"]["target_path"], "notes/pending.md");
        assert_eq!(detail["body_markdown"], "# Pending\n\nproposed body");

        let diff_resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/admin/pending-writes/{proposal_id}/diff?workspace=default&project=scratch"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(diff_resp.status(), StatusCode::OK);
        let body = to_bytes(diff_resp.into_body(), usize::MAX).await.unwrap();
        let diff: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(diff["diff"].as_str().unwrap().contains("proposed body"));

        let reject_resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/admin/pending-writes/{proposal_id}/reject?workspace=default&project=scratch"
                    ))
                    .header("content-type", "application/json")
                    .extension(ai_memory_core::ActorContext {
                        user: Some("reviewer".into()),
                        ..ai_memory_core::ActorContext::default()
                    })
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({"reason":"not now"})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reject_resp.status(), StatusCode::OK);
        assert!(
            store
                .reader
                .page_body_by_ids(_ws, _proj, "notes/pending.md")
                .await
                .unwrap()
                .is_none(),
            "reject must not write the target page"
        );
    }

    #[tokio::test]
    async fn pending_write_approve_writes_stored_body() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let (ws, proj, proposal_id) = stage_pending_write(
            &store,
            "default",
            "scratch",
            "notes/approved.md",
            "# Stored\n\nfrom stored proposal",
        )
        .await;
        wiki.write_auto_improve_sidecar(ws, proj, proposal_id)
            .await
            .unwrap();
        let router = admin_router(admin_state_for_store(&tmp, &store, wiki));

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/admin/pending-writes/{proposal_id}/approve?workspace=default&project=scratch"
                    ))
                    .extension(ai_memory_core::ActorContext {
                        user: Some("reviewer".into()),
                        ..ai_memory_core::ActorContext::default()
                    })
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let page = store
            .reader
            .page_body_by_ids(ws, proj, "notes/approved.md")
            .await
            .unwrap()
            .expect("approve writes target page");
        assert_eq!(page.body, "# Stored\n\nfrom stored proposal");
    }

    #[tokio::test]
    async fn read_page_missing_path_returns_404() {
        let (_tmp, router) = read_page_test_router();

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/read-page?workspace=default&project=audit&path=notes/nope.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn read_page_traversal_rejected_with_400() {
        let (_tmp, router) = read_page_test_router();

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/read-page?workspace=default&project=audit&path=../etc/passwd")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn read_page_without_path_or_query_returns_400() {
        let (_tmp, router) = read_page_test_router();

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/read-page?workspace=default&project=audit")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// Bug E regression: purge deletes the project's DB rows first, so by the
    /// time the admission chain fires, name-resolution from the (now gone)
    /// project row would yield an empty name and a name-based mirror would
    /// purge the wrong path. The handler must seed the admission context with
    /// the request's workspace/project names. We capture the `purge_project`
    /// webhook and assert it carries the real project name.
    #[tokio::test]
    async fn purge_project_admission_carries_the_project_name() {
        use ai_memory_wiki::{AdmissionChain, FailurePolicy, WebhookConfig};
        use axum::http::HeaderMap;
        use axum::routing::post;
        use std::sync::Mutex;

        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        let app = Router::new().route(
            "/sync",
            post(
                move |headers: HeaderMap, Json(payload): Json<serde_json::Value>| {
                    let cap = cap.clone();
                    async move {
                        if headers.get("X-Memory-Op").and_then(|v| v.to_str().ok())
                            == Some("purge_project")
                        {
                            *cap.lock().unwrap() = Some(
                                payload["ctx"]["project"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                            );
                        }
                        StatusCode::NO_CONTENT
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let chain = AdmissionChain::new(vec![WebhookConfig {
            name: "mirror".into(),
            url: format!("{base}/sync"),
            timeout_ms: 2_000,
            failure_policy: FailurePolicy::Ignore,
            events: vec![AdmissionOp::WritePage, AdmissionOp::PurgeProject],
            blocking: true,
        }])
        .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_admission_chain(chain)
            .with_store_reader(store.reader.clone());
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });

        post_write_page(&router, "default", "doomed", "notes/x.md", "bye").await;

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/purge-project")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "doomed",
                            "confirm": true,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "purge should succeed");

        // Let the async notify land.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            captured.lock().unwrap().as_deref(),
            Some("doomed"),
            "purge admission must carry the real project name, not an empty/_unscoped placeholder"
        );
    }

    /// Scope-guard / attribution regression: a destructive admin op must carry
    /// the authenticated actor into the admission context, so a scope-guard
    /// webhook can authorize by user. In prod a purge ran with an empty actor
    /// (`AdmissionContext { ..Default::default() }`) and scope-guard rejected it
    /// with `403 user '' not allowed to purge_project`.
    #[tokio::test]
    async fn purge_project_admission_carries_the_actor() {
        use ai_memory_core::ActorContext;
        use ai_memory_wiki::{AdmissionChain, FailurePolicy, WebhookConfig};
        use axum::http::HeaderMap;
        use axum::routing::post;
        use std::sync::Mutex;

        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        let app = Router::new().route(
            "/sync",
            post(
                move |headers: HeaderMap, Json(payload): Json<serde_json::Value>| {
                    let cap = cap.clone();
                    async move {
                        if headers.get("X-Memory-Op").and_then(|v| v.to_str().ok())
                            == Some("purge_project")
                        {
                            *cap.lock().unwrap() = Some(
                                payload["ctx"]["actor"]["user"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                            );
                        }
                        StatusCode::NO_CONTENT
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let chain = AdmissionChain::new(vec![WebhookConfig {
            name: "mirror".into(),
            url: format!("{base}/sync"),
            timeout_ms: 2_000,
            failure_policy: FailurePolicy::Ignore,
            events: vec![AdmissionOp::WritePage, AdmissionOp::PurgeProject],
            blocking: true,
        }])
        .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_admission_chain(chain)
            .with_store_reader(store.reader.clone());
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });

        post_write_page(&router, "default", "doomed", "notes/x.md", "bye").await;

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/purge-project")
                    .header("content-type", "application/json")
                    .extension(ActorContext {
                        user: Some("alice".into()),
                        ..ActorContext::default()
                    })
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "default",
                            "project": "doomed",
                            "confirm": true,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "purge should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            captured.lock().unwrap().as_deref(),
            Some("alice"),
            "purge admission must carry the authenticated actor (empty actor → scope-guard 403 in prod)"
        );
    }

    /// End-to-end reproduction of the prod scope-guard scenario: a
    /// `Reject`-policy admission webhook that authorizes `purge_project` by
    /// `ctx.actor.user` (the real scope-guard shape). Before the fix the handler
    /// sent an empty actor, so the guard returned 403 and the purge 500'd; with
    /// the actor propagated, a matching user is allowed and the purge succeeds.
    /// Both branches run in one test so the second (allowed) purge returning
    /// `200` — not `404` — also proves the first (denied) purge left the project
    /// intact.
    #[tokio::test]
    async fn scope_guard_blocks_purge_without_actor_allows_with_actor() {
        use ai_memory_core::ActorContext;
        use ai_memory_wiki::{AdmissionChain, FailurePolicy, WebhookConfig};
        use axum::routing::post;

        // scope-guard emulation: allow `purge_project` only when the admission
        // payload's actor is "alice"; deny (403) otherwise.
        let app = Router::new().route(
            "/guard",
            post(|Json(payload): Json<serde_json::Value>| async move {
                if payload["ctx"]["actor"]["user"].as_str() == Some("alice") {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::FORBIDDEN
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let chain = AdmissionChain::new(vec![WebhookConfig {
            name: "scope-guard".into(),
            url: format!("{base}/guard"),
            timeout_ms: 2_000,
            failure_policy: FailurePolicy::Reject,
            events: vec![AdmissionOp::PurgeProject],
            blocking: true,
        }])
        .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_admission_chain(chain)
            .with_store_reader(store.reader.clone());
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });

        post_write_page(&router, "default", "doomed", "notes/x.md", "bye").await;

        let purge_req = |actor: Option<&'static str>| {
            let mut builder = Request::builder()
                .method("POST")
                .uri("/admin/purge-project")
                .header("content-type", "application/json");
            if let Some(user) = actor {
                builder = builder.extension(ActorContext {
                    user: Some(user.into()),
                    ..ActorContext::default()
                });
            }
            builder
                .body(Body::from(
                    serde_json::to_vec(&serde_json::json!({
                        "workspace": "default",
                        "project": "doomed",
                        "confirm": true,
                    }))
                    .unwrap(),
                ))
                .unwrap()
        };

        // Case A — empty actor → scope-guard 403 → handler 500, project intact.
        let denied = router.clone().oneshot(purge_req(None)).await.unwrap();
        assert_eq!(
            denied.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "empty actor must be rejected by scope-guard (the prod bug)"
        );

        // Case B — actor=alice → scope-guard 204 → purge succeeds (200). A 200
        // here (not 404) also proves Case A left the project intact.
        let allowed = router
            .clone()
            .oneshot(purge_req(Some("alice")))
            .await
            .unwrap();
        assert_eq!(
            allowed.status(),
            StatusCode::OK,
            "matching actor must be authorized and the purge succeed"
        );
    }

    /// Merge moves copy source pages through `Wiki::write_page` before purging
    /// the source. That copy path must carry the authenticated actor too;
    /// otherwise a scope-guard webhook that gates `write_page` by user still
    /// rejects `/admin/move-project` even though purge/move admission was fixed.
    #[tokio::test]
    async fn move_project_merge_copy_admission_carries_the_actor() {
        use ai_memory_core::ActorContext;
        use ai_memory_wiki::{AdmissionChain, FailurePolicy, WebhookConfig};
        use axum::routing::post;

        let app = Router::new().route(
            "/guard",
            post(|Json(payload): Json<serde_json::Value>| async move {
                if payload["ctx"]["actor"]["user"].as_str() == Some("alice") {
                    StatusCode::NO_CONTENT
                } else {
                    StatusCode::FORBIDDEN
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let chain = AdmissionChain::new(vec![WebhookConfig {
            name: "scope-guard".into(),
            url: format!("{base}/guard"),
            timeout_ms: 2_000,
            failure_policy: FailurePolicy::Reject,
            events: vec![AdmissionOp::WritePage, AdmissionOp::PurgeProject],
            blocking: true,
        }])
        .unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_admission_chain(chain)
            .with_store_reader(store.reader.clone());
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });

        post_write_page_with_actor(
            &router,
            "src",
            "merged",
            "notes/source.md",
            "source body",
            Some("alice"),
        )
        .await;
        // Same project name in the destination workspace forces the copy-purge
        // merge path instead of the true-move path.
        post_write_page_with_actor(
            &router,
            "dst",
            "merged",
            "notes/existing.md",
            "existing body",
            Some("alice"),
        )
        .await;

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/move-project")
                    .header("content-type", "application/json")
                    .extension(ActorContext {
                        user: Some("alice".into()),
                        ..ActorContext::default()
                    })
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "from_workspace": "src",
                            "project": "merged",
                            "to_workspace": "dst",
                            "confirm": true,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK, "merge move should succeed");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["merged_into_existing"], true);
        assert_eq!(json["moved_via"], "copy-purge");
        assert_eq!(json["source_purged"], true);
    }

    #[tokio::test]
    async fn move_project_guard_and_true_move_retarget_keyed_active_entries() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let active =
            ai_memory_core::ActiveProject::with_mode(ai_memory_core::ActiveProjectMode::PerActor);
        let mut state = admin_state_for_store(&tmp, &store, wiki);
        state.active_project = active.clone();
        let router = admin_router(state);

        post_write_page(&router, "src", "live", "notes/source.md", "source body").await;
        let src_ws = store
            .reader
            .find_workspace("src".to_string())
            .await
            .unwrap()
            .unwrap();
        let src_proj = store
            .reader
            .find_project(src_ws, "live".to_string())
            .await
            .unwrap()
            .unwrap();
        let actor = ai_memory_core::ActorKey {
            user: Some("alice".into()),
            session_id: Some("session-1".into()),
        };
        active.set_for(&actor, src_ws, src_proj, false);
        active.set(WorkspaceId::new(), ProjectId::new());

        let move_body = serde_json::json!({
            "from_workspace": "src",
            "project": "live",
            "to_workspace": "dst",
            "confirm": true,
        });
        let guarded = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/move-project")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&move_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(guarded.status(), StatusCode::CONFLICT);

        let forced_body = serde_json::json!({
            "from_workspace": "src",
            "project": "live",
            "to_workspace": "dst",
            "confirm": true,
            "force": true,
        });
        let moved = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/move-project")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&forced_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(moved.status(), StatusCode::OK);
        let dst_ws = store
            .reader
            .find_workspace("dst".to_string())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(active.get_for(&actor), Some((dst_ws, src_proj)));
    }

    #[tokio::test]
    async fn delete_workspace_clears_single_and_keyed_active_entries() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let active =
            ai_memory_core::ActiveProject::with_mode(ai_memory_core::ActiveProjectMode::PerActor);
        let mut state = admin_state_for_store(&tmp, &store, wiki);
        state.active_project = active.clone();
        let router = admin_router(state);

        post_write_page(&router, "doomed-ws", "p", "notes/x.md", "bye").await;
        let ws = store
            .reader
            .find_workspace("doomed-ws".to_string())
            .await
            .unwrap()
            .unwrap();
        let proj = store
            .reader
            .find_project(ws, "p".to_string())
            .await
            .unwrap()
            .unwrap();
        let actor = ai_memory_core::ActorKey {
            user: Some("alice".into()),
            session_id: Some("session-1".into()),
        };
        active.set_for(&actor, ws, proj, false);
        active.set(ws, proj);

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/delete-workspace")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "doomed-ws",
                            "force": true,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(active.get().is_none());
        assert!(active.get_for(&actor).is_none());
        assert!(!active.contains_workspace(ws));
    }

    #[tokio::test]
    async fn merge_workspace_folds_projects_and_deletes_empty_source() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let state = admin_state_for_store(&tmp, &store, wiki);
        let router = admin_router(state);

        // Two fresh projects in the source; the destination has neither.
        post_write_page(&router, "src", "alpha", "notes/a.md", "alpha body").await;
        post_write_page(&router, "src", "beta", "notes/b.md", "beta body").await;

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/merge-workspace")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "from": "src",
                            "to": "dst",
                            "confirm": true,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["source_workspace_deleted"], true);
        let merged = json["projects_merged"].as_array().unwrap();
        assert_eq!(merged.len(), 2);
        // A fresh destination → each project is a lossless true-move.
        assert!(merged.iter().all(|r| r["moved_via"] == "true-move"));

        // The source workspace is gone; both projects now live under dst.
        assert!(
            store
                .reader
                .find_workspace("src".to_string())
                .await
                .unwrap()
                .is_none()
        );
        let dst_ws = store
            .reader
            .find_workspace("dst".to_string())
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .reader
                .find_project(dst_ws, "alpha".to_string())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .reader
                .find_project(dst_ws, "beta".to_string())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn merge_workspace_requires_confirm() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let state = admin_state_for_store(&tmp, &store, wiki);
        let router = admin_router(state);

        post_write_page(&router, "src", "alpha", "notes/a.md", "alpha body").await;

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/merge-workspace")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "from": "src",
                            "to": "dst",
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Nothing moved: the source workspace + project are untouched.
        assert!(
            store
                .reader
                .find_workspace("src".to_string())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn merge_workspace_merges_colliding_project_by_content() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let state = admin_state_for_store(&tmp, &store, wiki);
        let router = admin_router(state);

        // `shared` exists in BOTH workspaces (different page paths) → the move
        // for that project takes the copy-purge merge path, not a true-move.
        // `solo` exists only in the source → lossless true-move.
        post_write_page(&router, "dst", "shared", "notes/dst.md", "dst body").await;
        post_write_page(&router, "src", "shared", "notes/src.md", "src body").await;
        post_write_page(&router, "src", "solo", "notes/solo.md", "solo body").await;

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/merge-workspace")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "from": "src",
                            "to": "dst",
                            "confirm": true,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["source_workspace_deleted"], true);
        let merged = json["projects_merged"].as_array().unwrap();
        assert_eq!(merged.len(), 2);
        let vias: std::collections::HashSet<&str> = merged
            .iter()
            .filter_map(|r| r["moved_via"].as_str())
            .collect();
        assert!(
            vias.contains("copy-purge"),
            "the colliding project merged by copy-purge"
        );
        assert!(
            vias.contains("true-move"),
            "the solo project was a true-move"
        );

        // Source is gone; the merged `shared` project holds BOTH pages.
        assert!(
            store
                .reader
                .find_workspace("src".to_string())
                .await
                .unwrap()
                .is_none()
        );
        let pages = store.reader.list_pages("dst", "shared").await.unwrap();
        let paths: std::collections::HashSet<String> = pages.into_iter().map(|p| p.path).collect();
        assert!(paths.contains("notes/dst.md"));
        assert!(paths.contains("notes/src.md"));
    }

    fn recording_scope_invalidator(
        seen: Arc<std::sync::Mutex<Vec<ScopeInvalidation>>>,
    ) -> ScopeInvalidator {
        Arc::new(move |target| {
            let seen = seen.clone();
            Box::pin(async move {
                // Prove admin handlers await the invalidator before returning;
                // constructing and dropping this future would not record.
                tokio::task::yield_now().await;
                seen.lock().unwrap().push(target);
            })
        })
    }

    #[tokio::test]
    async fn delete_workspace_awaits_workspace_cache_invalidation() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut state = admin_state_for_store(&tmp, &store, wiki);
        state.scope_invalidator = Some(recording_scope_invalidator(seen.clone()));
        let router = admin_router(state);

        post_write_page(&router, "doomed-ws", "p", "notes/x.md", "bye").await;
        let ws = store
            .reader
            .find_workspace("doomed-ws".to_string())
            .await
            .unwrap()
            .unwrap();

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/delete-workspace")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "workspace": "doomed-ws",
                            "force": true,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            *seen.lock().unwrap(),
            vec![ScopeInvalidation::Workspace(ws)]
        );
    }

    #[tokio::test]
    async fn copy_purge_move_clears_keyed_active_entries_and_awaits_project_invalidation() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_store_reader(store.reader.clone());
        let active =
            ai_memory_core::ActiveProject::with_mode(ai_memory_core::ActiveProjectMode::PerActor);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut state = admin_state_for_store(&tmp, &store, wiki);
        state.active_project = active.clone();
        state.scope_invalidator = Some(recording_scope_invalidator(seen.clone()));
        let router = admin_router(state);

        post_write_page(&router, "src", "merged", "notes/source.md", "source body").await;
        post_write_page(
            &router,
            "dst",
            "merged",
            "notes/existing.md",
            "existing body",
        )
        .await;
        let src_ws = store
            .reader
            .find_workspace("src".to_string())
            .await
            .unwrap()
            .unwrap();
        let src_proj = store
            .reader
            .find_project(src_ws, "merged".to_string())
            .await
            .unwrap()
            .unwrap();
        let actor = ai_memory_core::ActorKey {
            user: Some("alice".into()),
            session_id: Some("session-1".into()),
        };
        active.set_for(&actor, src_ws, src_proj, false);
        active.set(src_ws, src_proj);

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/move-project")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "from_workspace": "src",
                            "project": "merged",
                            "to_workspace": "dst",
                            "confirm": true,
                            "force": true,
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(active.get().is_none());
        assert!(active.get_for(&actor).is_none());
        assert_eq!(
            *seen.lock().unwrap(),
            vec![ScopeInvalidation::Project(src_proj)]
        );
    }

    // ── user-management endpoints (P1.4) ──────────────────────────

    /// Test router that has a token pepper configured (so user-management
    /// endpoints don't return 503), runs the standard auth middleware
    /// upstream so `Extension<AuthLevel>` is populated, and is reachable
    /// as Root via a fixed bearer token.
    fn user_admin_test_router(root_token: &'static str) -> (TempDir, Router) {
        use ai_memory_core::ActorContext;
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let pepper = ai_memory_store::TokenPepper::new("test-pepper-admin");
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: Some(pepper),
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });
        // Wrap in a middleware that stamps the AuthLevel ourselves —
        // the real auth middleware lives in ai-memory-cli, and this
        // crate can't depend on it. The wrapper matches the Bearer
        // header against `root_token`: match → Root, present-but-no-
        // match → User (so we can test 403), absent → Anonymous.
        let router = router.layer(axum::middleware::from_fn(
            move |mut req: Request<Body>, next: axum::middleware::Next| async move {
                let bearer = req
                    .headers()
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|h| h.to_str().ok())
                    .and_then(|s| s.strip_prefix("Bearer "))
                    .map(str::to_string);
                let level = match bearer.as_deref() {
                    Some(t) if t == root_token => ai_memory_core::AuthLevel::Root,
                    Some(_) => ai_memory_core::AuthLevel::User,
                    None => ai_memory_core::AuthLevel::Anonymous,
                };
                req.extensions_mut().insert(level);
                req.extensions_mut().insert(ActorContext::anonymous());
                next.run(req).await
            },
        ));
        (tmp, router)
    }

    /// Same AdminState as [`user_admin_test_router`], but AuthLevel comes from
    /// the production dual-auth Bearer lookup (`authenticate_bearer` +
    /// `api_credentials` hash), not a stub that stamps Root/User by string
    /// compare. Configured `root_token` is AuthLevel::Root; a DB native key
    /// authenticates as AuthLevel::User even when `users.role` is root.
    fn user_admin_dual_auth_router(root_token: &'static str) -> (TempDir, Router) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let pepper = ai_memory_store::TokenPepper::new("test-pepper-admin");
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: Some(pepper.clone()),
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });
        let auth_state = Arc::new(
            crate::auth::AuthState::new(Some(root_token.to_string())).with_multiuser(
                pepper,
                store.reader.clone(),
                store.writer.clone(),
            ),
        );
        let router = router.layer(axum::middleware::from_fn_with_state(
            auth_state,
            crate::human_auth::require_dual_auth,
        ));
        (tmp, router)
    }

    async fn post_disable_user(
        router: &Router,
        root_token: &str,
        username: &str,
    ) -> axum::http::Response<Body> {
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/users/{username}/disable"))
                    .header("authorization", format!("Bearer {root_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn patch_user(
        router: &Router,
        root_token: &str,
        username: &str,
        body: serde_json::Value,
    ) -> axum::http::Response<Body> {
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(format!("/admin/users/{username}"))
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {root_token}"))
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn post_create_user(
        router: &Router,
        root_token: &str,
        body: serde_json::Value,
    ) -> axum::http::Response<Body> {
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/users")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {root_token}"))
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn post_create_human_user(
        router: &Router,
        root_token: &str,
        body: serde_json::Value,
    ) -> axum::http::Response<Body> {
        router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/human-users")
                    .header("content-type", "application/json")
                    .header("authorization", format!("Bearer {root_token}"))
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn user_token_admin_status(router: &Router, token: &str) -> StatusCode {
        router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/users")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    fn admin_route_samples() -> Vec<(&'static str, &'static str, serde_json::Value)> {
        vec![
            ("POST", "/admin/backup", serde_json::Value::Null),
            (
                "POST",
                "/admin/bootstrap",
                serde_json::json!({
                    "workspace": "default",
                    "project": "scratch",
                    "sources": [],
                    "dry_run": true
                }),
            ),
            (
                "POST",
                "/admin/auto-improve",
                serde_json::json!({
                    "workspace": "default",
                    "project": "scratch",
                    "session_id": "00000000-0000-0000-0000-000000000000",
                    "dry_run": true
                }),
            ),
            (
                "POST",
                "/admin/auto-improve/report",
                serde_json::json!({"workspace": "default", "project": "scratch"}),
            ),
            ("GET", "/admin/status", serde_json::Value::Null),
            (
                "GET",
                "/admin/sessions/by-agent?workspace=default&project=scratch",
                serde_json::Value::Null,
            ),
            (
                "GET",
                "/admin/activity/by-client?since_days=7",
                serde_json::Value::Null,
            ),
            ("GET", "/admin/audit-contamination", serde_json::Value::Null),
            ("GET", "/admin/audit-log", serde_json::Value::Null),
            ("GET", "/admin/search?q=test", serde_json::Value::Null),
            (
                "GET",
                "/admin/read-page?workspace=default&project=scratch&path=notes/x.md",
                serde_json::Value::Null,
            ),
            ("POST", "/admin/reorg", serde_json::json!({"dry_run": true})),
            (
                "POST",
                "/admin/lint",
                serde_json::json!({"workspace": "default", "project": "scratch", "dry_run": true}),
            ),
            (
                "POST",
                "/admin/forget-sweep",
                serde_json::json!({"workspace": "default", "project": "scratch", "dry_run": true}),
            ),
            (
                "POST",
                "/admin/embed",
                serde_json::json!({"workspace": "default", "project": "scratch", "dry_run": true}),
            ),
            (
                "POST",
                "/admin/curator",
                serde_json::json!({"workspace": "default", "project": "scratch", "dry_run": true}),
            ),
            (
                "POST",
                "/admin/commit",
                serde_json::json!({"message": "test"}),
            ),
            ("GET", "/admin/checkpoints?limit=1", serde_json::Value::Null),
            (
                "GET",
                "/admin/pending-writes?workspace=default&project=scratch",
                serde_json::Value::Null,
            ),
            (
                "GET",
                "/admin/pending-writes/00000000-0000-0000-0000-000000000000?workspace=default&project=scratch",
                serde_json::Value::Null,
            ),
            (
                "GET",
                "/admin/pending-writes/00000000-0000-0000-0000-000000000000/diff?workspace=default&project=scratch",
                serde_json::Value::Null,
            ),
            (
                "POST",
                "/admin/pending-writes/00000000-0000-0000-0000-000000000000/approve?workspace=default&project=scratch",
                serde_json::Value::Null,
            ),
            (
                "POST",
                "/admin/pending-writes/00000000-0000-0000-0000-000000000000/reject?workspace=default&project=scratch",
                serde_json::json!({"reason": "no"}),
            ),
            (
                "POST",
                "/admin/restore-page",
                serde_json::json!({
                    "workspace": "default",
                    "project": "scratch",
                    "path": "notes/x.md",
                    "rev": "HEAD"
                }),
            ),
            (
                "POST",
                "/admin/purge-project",
                serde_json::json!({"workspace": "default", "project": "scratch", "confirm": true}),
            ),
            (
                "POST",
                "/admin/rename-project",
                serde_json::json!({"workspace": "default", "from": "scratch", "to": "renamed"}),
            ),
            (
                "POST",
                "/admin/move-project",
                serde_json::json!({
                    "from_workspace": "default",
                    "to_workspace": "archive",
                    "project": "scratch",
                    "confirm": true
                }),
            ),
            (
                "POST",
                "/admin/delete-workspace",
                serde_json::json!({"workspace": "default", "force": true}),
            ),
            (
                "POST",
                "/admin/rename-workspace",
                serde_json::json!({"from": "default", "to": "renamed"}),
            ),
            (
                "POST",
                "/admin/write-page",
                serde_json::json!({
                    "workspace": "default",
                    "project": "scratch",
                    "path": "notes/x.md",
                    "body": "body"
                }),
            ),
            (
                "POST",
                "/admin/delete-page",
                serde_json::json!({
                    "workspace": "default",
                    "project": "scratch",
                    "path": "notes/x.md"
                }),
            ),
            ("GET", "/admin/users", serde_json::Value::Null),
            (
                "POST",
                "/admin/users",
                serde_json::json!({"username": "alice"}),
            ),
            (
                "POST",
                "/admin/human-users",
                serde_json::json!({"username": "human"}),
            ),
            ("POST", "/admin/users/alice/expire", serde_json::Value::Null),
            ("POST", "/admin/users/alice/revive", serde_json::Value::Null),
            (
                "POST",
                "/admin/users/alice/rotate-token",
                serde_json::Value::Null,
            ),
            (
                "POST",
                "/admin/users/alice/disable",
                serde_json::Value::Null,
            ),
            ("POST", "/admin/users/alice/enable", serde_json::Value::Null),
            ("GET", "/admin/api-credentials", serde_json::Value::Null),
        ]
    }

    #[tokio::test]
    async fn multiuser_admin_routes_reject_db_user_tier() {
        let (_tmp, router) = user_admin_test_router("root-token");
        post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "alice"}),
        )
        .await;

        for (method, uri, payload) in admin_route_samples() {
            let mut builder = Request::builder()
                .method(method)
                .uri(uri)
                .header("authorization", "Bearer db-user-token");
            let body = if payload.is_null() {
                Body::empty()
            } else {
                builder = builder.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&payload).unwrap())
            };
            let resp = router
                .clone()
                .oneshot(builder.body(body).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "{method} {uri} must be root-only for DB users in multi-user mode"
            );
        }
    }

    #[tokio::test]
    async fn multiuser_admin_routes_reject_anonymous() {
        let (_tmp, router) = user_admin_test_router("root-token");
        post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "alice"}),
        )
        .await;
        for (method, uri, payload) in admin_route_samples() {
            let mut builder = Request::builder().method(method).uri(uri);
            let body = if payload.is_null() {
                Body::empty()
            } else {
                builder = builder.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&payload).unwrap())
            };
            let resp = router
                .clone()
                .oneshot(builder.body(body).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} must require authentication in multi-user mode"
            );
        }
    }

    #[tokio::test]
    async fn multiuser_operational_admin_routes_allow_root() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/status")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/activity/by-client?since_days=7")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // The scope does not exist in this fixture, so reaching the handler
        // produces 404. A rejected root request would instead be 401/403.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/sessions/by-agent?workspace=default&project=scratch")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn trusted_proxy_topology_gates_admin_without_db_users() {
        use ai_memory_core::ActorContext;

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: Some(ai_memory_store::TokenPepper::new("test-pepper-admin")),
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: true,
        })
        .layer(axum::middleware::from_fn(
            |mut req: Request<Body>, next: axum::middleware::Next| async move {
                let level = match req
                    .headers()
                    .get("x-test-auth-level")
                    .and_then(|value| value.to_str().ok())
                {
                    Some("root") => ai_memory_core::AuthLevel::Root,
                    Some("user") => ai_memory_core::AuthLevel::User,
                    _ => ai_memory_core::AuthLevel::Anonymous,
                };
                req.extensions_mut().insert(level);
                req.extensions_mut().insert(ActorContext::anonymous());
                next.run(req).await
            },
        ));

        for (level, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("user"), StatusCode::FORBIDDEN),
            (Some("root"), StatusCode::OK),
        ] {
            let mut request = Request::builder().uri("/admin/status");
            if let Some(level) = level {
                request = request.header("x-test-auth-level", level);
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "level={level:?}");
        }
    }

    #[tokio::test]
    async fn admin_mode_switches_after_first_user_without_restart() {
        let (_tmp, router) = user_admin_test_router("root-token");

        let anonymous_before = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous_before.status(), StatusCode::OK);

        let create = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "alice"}),
        )
        .await;
        assert_eq!(create.status(), StatusCode::OK);

        for (token, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("db-user-token"), StatusCode::FORBIDDEN),
            (Some("root-token"), StatusCode::OK),
        ] {
            let mut request = Request::builder().uri("/admin/status");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let response = router
                .clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "token {token:?}");
        }

        let expire = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/users/alice/disable")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(expire.status(), StatusCode::OK);

        let anonymous_after_expire = router
            .oneshot(
                Request::builder()
                    .uri("/admin/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(anonymous_after_expire.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_mode_store_read_failure_fails_closed() {
        use ai_memory_core::ActorContext;

        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let missing_db_reader =
            ai_memory_store::ReaderPool::new(&tmp.path().join("missing.sqlite"), 1).unwrap();
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: missing_db_reader,
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: Some(ai_memory_store::TokenPepper::new("test-pepper-admin")),
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        })
        .layer(axum::middleware::from_fn(
            |mut req: Request<Body>, next: axum::middleware::Next| async move {
                req.extensions_mut()
                    .insert(ai_memory_core::AuthLevel::Anonymous);
                req.extensions_mut().insert(ActorContext::anonymous());
                next.run(req).await
            },
        ));

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/admin/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("could not verify user configuration"));
    }

    #[tokio::test]
    async fn multiuser_workspace_admin_routes_reach_handler_for_root() {
        let (_tmp, router) = user_admin_test_router("root-token");
        for (uri, payload) in [
            (
                "/admin/delete-workspace",
                serde_json::json!({"workspace": "missing", "force": true}),
            ),
            (
                "/admin/rename-workspace",
                serde_json::json!({"from": "missing", "to": "renamed"}),
            ),
        ] {
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(uri)
                        .header("authorization", "Bearer root-token")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&payload).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_ne!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "{uri} rejected root auth"
            );
            assert_ne!(
                resp.status(),
                StatusCode::FORBIDDEN,
                "{uri} rejected root auth"
            );
        }
    }

    #[tokio::test]
    async fn create_user_happy_path_returns_token_once() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let resp = post_create_user(
            &router,
            "root-token",
            serde_json::json!({
                "username": "alice",
                "name": "Alice Smith",
                "email": "Alice@Example.com"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["user"]["username"], "alice");
        // Email was normalised to lowercase by NewUser::validate.
        assert_eq!(json["user"]["email"], "alice@example.com");
        assert_eq!(json["user"]["name"], "Alice Smith");
        // Plaintext token is surfaced exactly once.
        let token = json["token"].as_str().unwrap();
        assert_eq!(token.len(), 43);
        assert!(json["user"]["token_expired_at"].is_null());
        assert!(json.get("temporary_password").is_none());
    }

    #[tokio::test]
    async fn create_human_user_is_additive_and_returns_temporary_password_once() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let resp = post_create_human_user(
            &router,
            "root-token",
            serde_json::json!({
                "username": "alice",
                "role": "root"
            }),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["temporary_password"].as_str().unwrap().len() >= 12);
        assert_eq!(json["user"]["role"], "root");
        assert!(json["user"]["must_change_password"].as_bool().unwrap());
        assert!(json.get("token").is_none());
    }

    #[tokio::test]
    async fn deprecated_user_token_routes_drive_the_legacy_api_credential() {
        let (_tmp, router) = user_admin_dual_auth_router("root-token");
        let create = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "alice"}),
        )
        .await;
        assert_eq!(create.status(), StatusCode::OK);
        let body = to_bytes(create.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let original = json["token"].as_str().unwrap().to_string();
        assert_eq!(
            user_token_admin_status(&router, &original).await,
            StatusCode::FORBIDDEN,
            "active legacy token must authenticate as a non-root DB user"
        );

        let expire = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/users/alice/expire")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(expire.status(), StatusCode::OK);
        assert_eq!(
            user_token_admin_status(&router, &original).await,
            StatusCode::UNAUTHORIZED
        );

        let revive = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/users/alice/revive")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(revive.status(), StatusCode::OK);
        assert_eq!(
            user_token_admin_status(&router, &original).await,
            StatusCode::FORBIDDEN
        );

        let rotate = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/users/alice/rotate-token")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rotate.status(), StatusCode::OK);
        let body = to_bytes(rotate.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let rotated = json["token"].as_str().unwrap();
        assert_ne!(rotated, original);
        assert_eq!(
            user_token_admin_status(&router, &original).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            user_token_admin_status(&router, rotated).await,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn create_user_rejects_duplicate_username_with_409() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let _ = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "alice"}),
        )
        .await;
        let resp = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "alice"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn create_user_rejects_invalid_email_with_400() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let resp = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "alice", "email": "not-an-email"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_user_as_anonymous_returns_401() {
        let (_tmp, router) = user_admin_test_router("root-token");
        // No Authorization header → middleware stamps Anonymous tier.
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/users")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({"username": "alice"})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn create_user_as_user_tier_returns_403() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let resp = post_create_user(
            &router,
            "not-the-root-token",
            serde_json::json!({"username": "alice"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn list_users_returns_added_users_in_insertion_order() {
        let (_tmp, router) = user_admin_test_router("root-token");
        for n in ["alice", "bob", "carol"] {
            let _ =
                post_create_user(&router, "root-token", serde_json::json!({"username": n})).await;
        }
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/users")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let users = json["users"].as_array().unwrap();
        assert_eq!(users.len(), 3);
        assert_eq!(users[0]["username"], "alice");
        assert_eq!(users[1]["username"], "bob");
        assert_eq!(users[2]["username"], "carol");
        // Tokens are NEVER surfaced by the list endpoint.
        for u in users {
            assert!(
                u.get("token").is_none() && u.get("temporary_password").is_none(),
                "list must not leak secrets"
            );
        }
    }

    #[tokio::test]
    async fn disable_then_enable_round_trips() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let _ = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "alice"}),
        )
        .await;

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/users/alice/disable")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["user"]["disabled_at"].is_i64());

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/users/alice/enable")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["user"]["disabled_at"].is_null());
    }

    #[tokio::test]
    async fn disable_unknown_user_returns_404() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/users/ghost/disable")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rotate_api_credential_issues_a_distinct_secret() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let _ = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "alice"}),
        )
        .await;
        let create_resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/api-credentials")
                    .header("authorization", "Bearer root-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "alice",
                            "label": "laptop"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create_resp.status(), StatusCode::OK);
        let body = to_bytes(create_resp.into_body(), usize::MAX).await.unwrap();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let original_token = created["token"].as_str().unwrap().to_string();
        let id = created["credential"]["id"].as_str().unwrap();
        assert!(original_token.starts_with("aim_"));

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/admin/api-credentials/{id}/rotate"))
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let new_token = json["token"].as_str().unwrap();
        assert!(new_token.starts_with("aim_"));
        assert_ne!(new_token, original_token, "rotate must change the secret");
    }

    #[tokio::test]
    async fn create_human_user_works_without_pepper_but_api_keys_return_503() {
        // Humans do not need a pepper. Native API keys still do.
        use ai_memory_core::ActorContext;
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let router = admin_router(AdminState {
            ingest_metrics: std::sync::Arc::new(ai_memory_core::IngestMetrics::default()),
            writer: store.writer.clone(),
            reader: store.reader.clone(),
            wiki,
            llm: None,
            auto_improve_require_approval: false,
            auto_improve_review_config: Default::default(),
            embedder: None,
            provider_health: ProviderHealth::default(),
            decay_params: DecayParams::default(),
            data_dir: tmp.path().to_path_buf(),
            db_path: store.db_path().to_path_buf(),
            bind: "127.0.0.1:49374".to_string(),
            home_dir: None,
            bootstrap_lock: Arc::new(tokio::sync::Mutex::new(())),
            token_pepper: None,
            active_project: ai_memory_core::ActiveProject::new(),
            scope_invalidator: None,
            trusted_proxy_identity: false,
        });
        // Inject Root so the human create reaches its handler; only native
        // API credentials require the pepper.
        let router = router.layer(axum::middleware::from_fn(
            |mut req: Request<Body>, next: axum::middleware::Next| async move {
                req.extensions_mut().insert(ai_memory_core::AuthLevel::Root);
                req.extensions_mut().insert(ActorContext::anonymous());
                next.run(req).await
            },
        ));

        let created = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/human-users")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({"username": "alice"})).unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);

        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/api-credentials")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "alice",
                            "label": "cli"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let _ = tmp;
    }

    #[tokio::test]
    async fn patch_demotion_of_last_recoverable_root_returns_409() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let created = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "keeper", "role": "root"}),
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK);
        let body = to_bytes(created.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["user"]["role"], "root");
        assert_eq!(json["user"]["has_password"].as_bool(), Some(true));

        let resp = patch_user(
            &router,
            "root-token",
            "keeper",
            serde_json::json!({"role": "user"}),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or("")
                .contains("last recoverable root"),
            "409 body must surface the store InvalidState mapping; got {json:?}"
        );
    }

    #[tokio::test]
    async fn disable_of_last_recoverable_root_returns_409() {
        let (_tmp, router) = user_admin_test_router("root-token");
        let created = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "keeper", "role": "root"}),
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK);

        let resp = post_disable_user(&router, "root-token", "keeper").await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(
            json["error"]
                .as_str()
                .unwrap_or("")
                .contains("last recoverable root"),
            "409 body must surface the store InvalidState mapping; got {json:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_root_removals_leave_one_recoverable_root() {
        let (_tmp, router) = user_admin_test_router("root-token");
        for name in ["alice", "bob"] {
            let created = post_create_user(
                &router,
                "root-token",
                serde_json::json!({"username": name, "role": "root"}),
            )
            .await;
            assert_eq!(created.status(), StatusCode::OK, "setup {name}");
        }

        let (demote, disable) = tokio::join!(
            patch_user(
                &router,
                "root-token",
                "alice",
                serde_json::json!({"role": "user"}),
            ),
            post_disable_user(&router, "root-token", "bob"),
        );
        let statuses = [demote.status(), disable.status()];
        let ok = statuses.iter().filter(|s| **s == StatusCode::OK).count();
        assert!(
            ok <= 1,
            "at most one concurrent root-removal may succeed; got {statuses:?}"
        );
        assert!(
            statuses
                .iter()
                .all(|s| *s == StatusCode::OK || *s == StatusCode::CONFLICT),
            "root-removal must be 200 or 409; got {statuses:?}"
        );

        let listed = router
            .oneshot(
                Request::builder()
                    .uri("/admin/users")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let body = to_bytes(listed.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let recoverable = json["users"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|u| {
                u["role"] == "root"
                    && u["disabled_at"].is_null()
                    && u["has_password"].as_bool() == Some(true)
            })
            .count();
        assert!(
            recoverable >= 1,
            "a recoverable root must remain after concurrent removals; got {json:?}"
        );
    }

    #[tokio::test]
    async fn native_api_token_for_root_role_user_cannot_manage_users() {
        let (_tmp, router) = user_admin_dual_auth_router("root-token");
        let created = post_create_user(
            &router,
            "root-token",
            serde_json::json!({"username": "boss", "role": "root"}),
        )
        .await;
        assert_eq!(created.status(), StatusCode::OK);
        let body = to_bytes(created.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["user"]["role"], "root");

        let issued = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/api-credentials")
                    .header("authorization", "Bearer root-token")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "username": "boss",
                            "label": "legacy-user-token"
                        }))
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(issued.status(), StatusCode::OK);
        let body = to_bytes(issued.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let native_token = json["token"].as_str().unwrap();
        assert!(native_token.starts_with("aim_"));

        let as_native = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/users")
                    .header("authorization", format!("Bearer {native_token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            as_native.status(),
            StatusCode::FORBIDDEN,
            "role=root must not elevate a looked-up native API credential"
        );

        let unknown = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/users")
                    .header("authorization", "Bearer aim_not-a-stored-credential")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            unknown.status(),
            StatusCode::UNAUTHORIZED,
            "unknown bearer must 401 through real lookup, not a User stub"
        );

        let as_root = router
            .oneshot(
                Request::builder()
                    .uri("/admin/users")
                    .header("authorization", "Bearer root-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(as_root.status(), StatusCode::OK);
    }

    // ---------------------------------------------------------------------
    // delete-page
    // ---------------------------------------------------------------------

    /// Helper: POST /admin/delete-page and return (status, body json).
    async fn post_delete_page(
        router: &Router,
        ws: &str,
        project: &str,
        path: &str,
    ) -> (StatusCode, serde_json::Value) {
        let req_body = serde_json::json!({
            "workspace": ws,
            "project": project,
            "path": path,
        });
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/delete-page")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&req_body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::json!({}));
        (status, json)
    }

    #[tokio::test]
    async fn delete_page_removes_existing_page() {
        let (_tmp, router) = read_page_test_router();
        post_write_page(&router, "default", "audit", "notes/doomed.md", "bye body").await;

        // Confirm the page is reachable before delete.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/admin/read-page?workspace=default&project=audit&path=notes/doomed.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "setup precondition: page must exist before delete"
        );

        let (status, json) = post_delete_page(&router, "default", "audit", "notes/doomed.md").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["path"], "notes/doomed.md");
        assert_eq!(json["deleted"], true);

        // Read-back must now 404.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/admin/read-page?workspace=default&project=audit&path=notes/doomed.md")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "page must be gone after delete"
        );
    }

    /// A delete request whose `(workspace, project)` doesn't resolve to a
    /// real `(WorkspaceId, ProjectId)` must NOT report success. The shared
    /// `resolve_ws_proj` returns the unresolved-scope error; the handler
    /// surfaces it as a 4xx/5xx — never as `deleted: true`. This guards the
    /// Bug 5 regression where MCP `memory_delete_page` returned `true` for
    /// a scope it never touched.
    #[tokio::test]
    async fn delete_page_unknown_workspace_does_not_fake_success() {
        let (_tmp, router) = read_page_test_router();
        let (status, json) =
            post_delete_page(&router, "no-such-ws", "audit", "notes/whatever.md").await;
        assert_ne!(
            status,
            StatusCode::OK,
            "delete on unresolved scope must not return 200/deleted=true; got body {json:?}",
        );
        assert!(
            json.get("deleted").and_then(|v| v.as_bool()) != Some(true),
            "body must not claim deleted=true on unresolved scope; got {json:?}"
        );
    }

    /// `Wiki::delete_page` is idempotent for a path that doesn't exist
    /// inside an EXISTING (workspace, project) — the file is just not there
    /// to quarantine. The handler reports `deleted: true` (i.e. "the call
    /// succeeded") rather than 404, matching the documented MCP semantics.
    #[tokio::test]
    async fn delete_page_idempotent_for_missing_file_in_existing_scope() {
        let (_tmp, router) = read_page_test_router();
        // Seed the project so (workspace, project) resolves, but skip the
        // page we'll try to delete.
        post_write_page(&router, "default", "audit", "notes/keep.md", "keeper").await;

        let (status, json) =
            post_delete_page(&router, "default", "audit", "notes/never-existed.md").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["deleted"], true);
    }

    #[tokio::test]
    async fn delete_page_traversal_rejected_with_422() {
        let (_tmp, router) = read_page_test_router();
        let (status, _) = post_delete_page(&router, "default", "audit", "../etc/passwd").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    }
}
