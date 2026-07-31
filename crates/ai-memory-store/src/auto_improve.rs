//! SQLite persistence for the auto-improvement loop.
//!
//! The consolidate crate reviews a finished session and produces *proposals*
//! (create/update a wiki page). This module stores those proposals with a
//! full audit trail so an operator can approve, reject, or fail them later:
//!
//! - `auto_improve_runs` — one row per review pass, with provider/model,
//!   warnings, and the rejected-candidate list for telemetry.
//! - `auto_improve_proposals` — staged page edits plus a snapshot of the
//!   target page at stage time (`*_at_stage` columns). Approval re-checks
//!   that snapshot and returns [`ApproveAutoImproveProposalResult::Conflict`]
//!   instead of clobbering a page that changed since staging.
//! - `auto_improve_proposal_events` — append-only status history per
//!   proposal (`staged` / `approved` / `rejected` / `failed` / `conflict`).
//! - `auto_improve_rejections` — normalized rejection records with a
//!   whitespace/case-insensitive fingerprint so the telemetry report can
//!   spot the same proposal being re-staged and re-rejected.
//! - `auto_improve_scheduler_state` / `_claims` — the session-end scheduler
//!   watermark and per-session claims that make the background reviewer
//!   idempotent across restarts.
//!
//! All mutations run inside a transaction on the caller's connection; the
//! writer actor owns that connection, so the single-writer invariant holds.

use std::str::FromStr;

use ai_memory_core::{
    ActorContext, AutoImproveProposalId, AutoImproveRunId, NewPage, PageId, PagePath, ProjectId,
    SessionId, UserId, WorkspaceId,
};
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{StoreError, StoreResult};
use crate::ops;

/// Lifecycle status of a staged proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoImproveProposalStatus {
    /// Staged and awaiting an operator decision.
    Pending,
    /// Approved and applied — `applied_page_id` points at the written page.
    Approved,
    /// Rejected by an operator with a reason.
    Rejected,
    /// Approval was attempted but the target page changed since staging.
    Conflict,
    /// Application failed after approval (e.g. the page write errored).
    Failed,
}

impl AutoImproveProposalStatus {
    /// Snake-case string stored in the `status` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Conflict => "conflict",
            Self::Failed => "failed",
        }
    }
}

impl FromStr for AutoImproveProposalStatus {
    type Err = StoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "rejected" => Ok(Self::Rejected),
            "conflict" => Ok(Self::Conflict),
            "failed" => Ok(Self::Failed),
            other => Err(StoreError::MalformedRecord(format!(
                "unknown auto-improve proposal status: {other}"
            ))),
        }
    }
}

/// What a proposal wants to do to its target page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoImproveProposalOperation {
    /// Create a page that must not exist yet at stage time.
    Create,
    /// Rewrite a page that must already exist at stage time.
    Update,
}

impl AutoImproveProposalOperation {
    /// Snake-case string stored in the `operation` column.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
        }
    }
}

impl FromStr for AutoImproveProposalOperation {
    type Err = StoreError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "create" => Ok(Self::Create),
            "update" => Ok(Self::Update),
            other => Err(StoreError::MalformedRecord(format!(
                "unknown auto-improve proposal operation: {other}"
            ))),
        }
    }
}

/// One review pass to persist: run metadata plus its staged proposals.
#[derive(Debug, Clone)]
pub struct StageAutoImproveRun {
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Session the review covered, when session-scoped. Must belong to the
    /// same workspace/project or staging fails.
    pub session_id: Option<SessionId>,
    /// LLM provider name used for the review, for telemetry.
    pub provider: Option<String>,
    /// LLM model id used for the review, for telemetry.
    pub model: Option<String>,
    /// Reviewer's one-paragraph run summary.
    pub summary: Option<String>,
    /// Reviewer warnings (JSON array) surfaced in reports.
    pub warnings_json: serde_json::Value,
    /// Candidates the reviewer itself rejected (JSON array); each entry with
    /// a non-empty `reason` also lands in `auto_improve_rejections`.
    pub rejected_candidates_json: serde_json::Value,
    /// Effective review config snapshot for reproducibility.
    pub config_json: serde_json::Value,
    /// Actor attribution recorded on the run and its `staged` events.
    pub proposal_actor: ActorContext,
    /// Operator that staged this run, as the username the request authenticated
    /// or was proxy-asserted under.
    ///
    /// A username rather than a `users(id)`: the majority of identified callers
    /// on a shared server are asserted by an authenticating proxy and have no
    /// row. `None` for a single-operator server and for any unattributed
    /// caller, which keeps the one-pending-per-target rule behaving exactly as
    /// before (see V42).
    pub staged_by_actor_user: Option<String>,
    /// Page edits to stage as pending proposals.
    pub proposals: Vec<NewAutoImproveProposal>,
}

/// One page edit to stage, before it gets an id or a status.
#[derive(Debug, Clone)]
pub struct NewAutoImproveProposal {
    /// Create a new page or update an existing one.
    pub operation: AutoImproveProposalOperation,
    /// Wiki path of the page this proposal targets.
    pub target_path: PagePath,
    /// Proposal category (e.g. `learning`, maintenance kinds) for telemetry.
    pub kind: String,
    /// Human-readable proposal title.
    pub title: String,
    /// Reviewer confidence in `0.0..=1.0`.
    pub confidence: f64,
    /// Why the reviewer proposes this edit.
    pub rationale: String,
    /// Supporting evidence (JSON) shown to the deciding operator.
    pub evidence_json: serde_json::Value,
    /// Full proposed page body (also the materialized result for patches).
    pub body_markdown: String,
    /// SHA-256 of the staged `_pending/` artifact file, when one was written.
    pub artifact_sha256: Option<[u8; 32]>,
    /// `full_page` (default when `None`) or `patch`.
    pub edit_mode: Option<String>,
    /// Patch operations (JSON) — required when `edit_mode == "patch"`.
    pub patch_json: Option<serde_json::Value>,
    /// SHA-256 of the base body the patch was materialized against —
    /// required for patches; staging fails if the live target differs.
    pub expected_base_body_sha256: Option<[u8; 32]>,
}

/// Ids assigned by [`stage_run`].
#[derive(Debug, Clone, Serialize)]
pub struct StagedAutoImproveRun {
    /// Id of the recorded run row.
    pub run_id: AutoImproveRunId,
    /// Ids of the staged proposals, in input order.
    pub proposal_ids: Vec<AutoImproveProposalId>,
    /// Proposals that could not be staged because something is already pending
    /// for the same target, reported instead of silently dropped.
    ///
    /// A colliding proposal used to abort the whole transaction, losing the run
    /// row, the non-colliding proposals beside it, and the (paid) LLM call that
    /// produced them.
    pub skipped: Vec<SkippedProposal>,
}

/// One proposal that was not staged, and why.
///
/// `Deserialize` because the HTTP clients (the CLI) have to read this back off
/// the wire to show it; a report the operator never sees is the silent drop
/// again, one hop further out.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkippedProposal {
    /// Wiki path the proposal targeted.
    pub target_path: String,
    /// Human-readable reason, safe to surface to an operator.
    pub reason: String,
}

/// List-view projection of a proposal (no bodies or event history).
#[derive(Debug, Clone, Serialize)]
pub struct AutoImproveProposalSummary {
    /// Proposal id.
    pub id: AutoImproveProposalId,
    /// Run that staged the proposal.
    pub run_id: AutoImproveRunId,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Current lifecycle status.
    pub status: AutoImproveProposalStatus,
    /// Create vs update.
    pub operation: AutoImproveProposalOperation,
    /// Wiki path of the targeted page.
    pub target_path: PagePath,
    /// Proposal category for telemetry.
    pub kind: String,
    /// Human-readable proposal title.
    pub title: String,
    /// Reviewer confidence in `0.0..=1.0`.
    pub confidence: f64,
    /// Stage time (Unix microseconds).
    pub staged_at: i64,
    /// Decision time (Unix microseconds), once decided.
    pub decided_at: Option<i64>,
}

/// Full proposal record: summary plus bodies, stage-time target snapshot,
/// decision attribution, and the append-only event history.
#[derive(Debug, Clone, Serialize)]
pub struct AutoImproveProposalDetail {
    /// The list-view fields.
    pub summary: AutoImproveProposalSummary,
    /// Why the reviewer proposed this edit.
    pub rationale: String,
    /// Supporting evidence (JSON) shown to the deciding operator.
    pub evidence_json: serde_json::Value,
    /// Full proposed page body.
    pub body_markdown: String,
    /// SHA-256 of `body_markdown`, computed at stage time.
    pub body_sha256: [u8; 32],
    /// `_pending/auto-improve/<id>.md` artifact path for this proposal.
    pub artifact_path: String,
    /// SHA-256 of the staged artifact file, when one was written.
    pub artifact_sha256: Option<[u8; 32]>,
    /// Latest page id of the target at stage time (`None` for creates).
    pub target_latest_page_id_at_stage: Option<PageId>,
    /// Target body hash at stage time — approval compares against this to
    /// detect a page that changed after staging.
    pub target_body_sha256_at_stage: Option<[u8; 32]>,
    /// Target `updated_at` at stage time (Unix microseconds).
    pub target_updated_at_at_stage: Option<i64>,
    /// Operator-supplied reason for a reject/fail/conflict decision.
    pub decision_reason: Option<String>,
    /// Deciding user, when the decision came from an identified account.
    pub decided_by_author_id: Option<UserId>,
    /// Full actor context (JSON) recorded at decision time.
    pub decided_by_actor_json: Option<serde_json::Value>,
    /// Page written by an approval.
    pub applied_page_id: Option<PageId>,
    /// Wiki git checkpoint recorded alongside an approval, when available.
    pub checkpoint: Option<String>,
    /// `full_page` or `patch`.
    pub edit_mode: String,
    /// Patch operations (JSON) for `patch` proposals.
    pub patch_json: Option<serde_json::Value>,
    /// Base body hash the patch was generated against.
    pub expected_base_body_sha256: Option<[u8; 32]>,
    /// Base body hash the staged `body_markdown` was materialized from.
    pub materialized_base_body_sha256: Option<[u8; 32]>,
    /// Operator whose session produced this proposal, `None` for an
    /// unattributed run (single-operator server, or the unattended scheduler on
    /// a session nobody was named on).
    ///
    /// Reviewer-facing — V42 added the column so a reviewer could tell whose
    /// suggestion they were looking at — and load-bearing: it is the attribution
    /// the approval-side slot guard checks the target against, because the
    /// approver is a reviewer, not the owner of the page.
    pub staged_by_actor_user: Option<String>,
    /// Status history, oldest first.
    pub events: Vec<AutoImproveProposalEvent>,
}

/// One append-only status-history entry for a proposal.
#[derive(Debug, Clone, Serialize)]
pub struct AutoImproveProposalEvent {
    /// Autoincrement row id (orders events).
    pub id: i64,
    /// Proposal this event belongs to.
    pub proposal_id: AutoImproveProposalId,
    /// Event name: `staged`, `approved`, `rejected`, `failed`, or `conflict`.
    pub event: String,
    /// Actor context (JSON) that caused the event.
    pub actor_json: serde_json::Value,
    /// Acting user, when identified.
    pub author_id: Option<UserId>,
    /// Event-specific payload (e.g. `reason`, `applied_page_id`).
    pub detail_json: serde_json::Value,
    /// Event time (Unix microseconds).
    pub at: i64,
}

/// Normalized rejection record used by the telemetry report to spot the
/// same proposal being re-staged and re-rejected.
#[derive(Debug, Clone, Serialize)]
pub struct AutoImproveRejectionSummary {
    /// Rejection row id (UUID string).
    pub id: String,
    /// Owning workspace.
    pub workspace_id: WorkspaceId,
    /// Owning project.
    pub project_id: ProjectId,
    /// Targeted wiki path, when the rejected candidate named one.
    pub target_path: Option<String>,
    /// Proposal category, when known.
    pub kind: Option<String>,
    /// Create vs update, when known.
    pub operation: Option<String>,
    /// `full_page` / `patch`, when known.
    pub edit_mode: Option<String>,
    /// Why the candidate/proposal was rejected.
    pub reason: String,
    /// Whitespace/case-insensitive SHA-256 over the identifying fields —
    /// equal fingerprints mean "the same rejection happened again".
    pub normalized_fingerprint: String,
    /// One-line description (title, summary, or the reason itself).
    pub summary: String,
    /// Original candidate/evidence payload (JSON).
    pub evidence_json: serde_json::Value,
    /// Run the rejection came from, when known.
    pub source_run_id: Option<AutoImproveRunId>,
    /// Proposal the rejection came from (`None` for reviewer-side rejects).
    pub source_proposal_id: Option<AutoImproveProposalId>,
    /// Record time (Unix microseconds).
    pub created_at: i64,
}

/// Aggregate telemetry for auto-improvement runs in one scope.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoImproveTelemetryAggregate {
    /// Number of auto-improve runs in the window.
    pub run_count: usize,
    /// Number of runs that staged at least one learning proposal.
    pub runs_with_learning_proposals: usize,
    /// Learning proposal counts by status.
    pub proposals_by_status: Vec<AutoImproveTelemetryCount>,
    /// Learning proposal counts by operation.
    pub proposals_by_operation: Vec<AutoImproveTelemetryCount>,
    /// Learning proposal counts by edit mode.
    pub proposals_by_edit_mode: Vec<AutoImproveTelemetryCount>,
    /// Learning proposal counts by kind.
    pub proposals_by_kind: Vec<AutoImproveTelemetryCount>,
    /// Maintenance/report proposal counts by kind.
    pub maintenance_proposals_by_kind: Vec<AutoImproveTelemetryCount>,
    /// Most frequently targeted learning pages.
    pub top_targets: Vec<AutoImproveTelemetryCount>,
    /// Rejection counts by reason.
    pub rejections_by_reason: Vec<AutoImproveTelemetryCount>,
    /// Repeated rejection fingerprints.
    pub repeated_rejection_fingerprints: Vec<AutoImproveTelemetryCount>,
    /// Most common rejected targets.
    pub rejected_targets: Vec<AutoImproveTelemetryCount>,
}

/// Generic `(key, count)` telemetry row.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AutoImproveTelemetryCount {
    /// Aggregated key; field-specific meaning depends on the source vector.
    pub key: String,
    /// Number of rows matching the key.
    pub count: usize,
}

/// Reject a pending proposal with a reason.
#[derive(Debug, Clone)]
pub struct RejectAutoImproveProposal {
    /// Owning workspace (scope check).
    pub workspace_id: WorkspaceId,
    /// Owning project (scope check).
    pub project_id: ProjectId,
    /// Proposal to reject; must currently be pending.
    pub proposal_id: AutoImproveProposalId,
    /// Operator-supplied rejection reason.
    pub reason: String,
    /// Actor attribution for the decision event.
    pub actor: ActorContext,
    /// Deciding user, when identified.
    pub author_id: Option<UserId>,
}

/// Mark a pending proposal failed (its application errored).
#[derive(Debug, Clone)]
pub struct FailAutoImproveProposal {
    /// Owning workspace (scope check).
    pub workspace_id: WorkspaceId,
    /// Owning project (scope check).
    pub project_id: ProjectId,
    /// Proposal to fail; must currently be pending.
    pub proposal_id: AutoImproveProposalId,
    /// What went wrong.
    pub reason: String,
    /// Actor attribution for the decision event.
    pub actor: ActorContext,
    /// Deciding user, when identified.
    pub author_id: Option<UserId>,
}

/// Approve a pending proposal and apply its page in the same transaction.
#[derive(Debug, Clone)]
pub struct ApproveAutoImproveProposal {
    /// Owning workspace (scope check).
    pub workspace_id: WorkspaceId,
    /// Owning project (scope check).
    pub project_id: ProjectId,
    /// Proposal to approve; must currently be pending.
    pub proposal_id: AutoImproveProposalId,
    /// Page to write on approval. Its scope, author, and path must match the
    /// proposal or approval fails before touching anything.
    pub page: NewPage,
    /// Actor attribution for the decision event.
    pub actor: ActorContext,
    /// Deciding user, when identified.
    pub author_id: Option<UserId>,
    /// Wiki git checkpoint to record alongside the approval.
    pub checkpoint: Option<String>,
}

/// Outcome of [`approve_proposal`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveAutoImproveProposalResult {
    /// The page was written and the proposal marked approved.
    Approved {
        /// Id of the written page version.
        page_id: PageId,
    },
    /// The target changed since staging; the proposal was marked `conflict`
    /// and nothing was written.
    Conflict,
    /// The proposal may not be applied at all — the write policy of the door
    /// that would apply it refuses this target. The proposal is marked `failed`
    /// and nothing was written.
    ///
    /// A verdict, not an error: [`approve_proposal`] never returns it, but the
    /// wiki's approval does (a slot target that is not the proposal owner's),
    /// and it must not abort the remaining approvals in the same run — the
    /// per-proposal channel staging collisions already use.
    Refused {
        /// Reviewer-facing reason, also stored as the proposal's
        /// `decision_reason`.
        reason: String,
    },
}

/// `_pending/auto-improve/<id>.md` — where a staged proposal's body artifact
/// lives in the wiki tree.
#[must_use]
pub fn artifact_path_for(proposal_id: AutoImproveProposalId) -> String {
    format!("_pending/auto-improve/{proposal_id}.md")
}

/// Ensure a scheduler-state row exists for the scope, seeding the watermark
/// at the newest already-ended session so pre-existing history is not
/// retroactively reviewed.
///
/// # Errors
/// Returns an error when the underlying SQLite statements fail.
pub fn ensure_scheduler_state(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let watermark_ended_at = conn.query_row(
        "SELECT COALESCE(MAX(ended_at), 0) FROM sessions \
         WHERE workspace_id = ?1 AND project_id = ?2 AND ended_at IS NOT NULL",
        params![workspace_id.as_bytes(), project_id.as_bytes()],
        |row| row.get::<_, i64>(0),
    )?;
    conn.execute(
        "INSERT INTO auto_improve_scheduler_state \
         (workspace_id, project_id, watermark_ended_at, initialized_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?4) \
         ON CONFLICT(workspace_id, project_id) DO NOTHING",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            watermark_ended_at,
            now,
        ],
    )?;
    Ok(())
}

/// Atomically claim one ended session for background review. Returns `true`
/// only for the first claimer: the insert requires the session to be past
/// the scope's watermark and not already covered by a run, so concurrent
/// schedulers and restarts cannot double-review a session.
///
/// # Errors
/// Returns an error when the underlying SQLite statements fail.
pub fn claim_scheduler_session(
    conn: &mut Connection,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    ended_at: i64,
) -> StoreResult<bool> {
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO auto_improve_scheduler_claims \
         (workspace_id, project_id, session_id, claimed_at) \
         SELECT ?1, ?2, ?3, ?4 \
         WHERE EXISTS ( \
             SELECT 1 FROM auto_improve_scheduler_state st \
             JOIN sessions s \
               ON s.workspace_id = st.workspace_id \
              AND s.project_id = st.project_id \
             WHERE st.workspace_id = ?1 \
               AND st.project_id = ?2 \
               AND s.id = ?3 \
               AND s.ended_at = ?5 \
               AND s.ended_at > st.watermark_ended_at \
         ) \
           AND NOT EXISTS ( \
               SELECT 1 FROM auto_improve_runs r \
               WHERE r.workspace_id = ?1 \
                 AND r.project_id = ?2 \
                 AND r.session_id = ?3 \
           )",
        params![
            workspace_id.as_bytes(),
            project_id.as_bytes(),
            session_id.as_bytes(),
            now,
            ended_at,
        ],
    )?;
    if inserted == 1 {
        tx.execute(
            "UPDATE auto_improve_scheduler_state \
             SET updated_at = ?3 \
             WHERE workspace_id = ?1 AND project_id = ?2",
            params![workspace_id.as_bytes(), project_id.as_bytes(), now],
        )?;
    }
    tx.commit()?;
    Ok(inserted == 1)
}

/// Persist one review run and stage its proposals as `pending`, all in one
/// transaction. Validates per-proposal preconditions (create targets must
/// not exist, update/patch targets must exist and — for patches — still
/// match the expected base hash) and snapshots the target page so approval
/// can detect later drift.
///
/// # Errors
/// Returns [`StoreError::InvalidState`] when a precondition fails, or an
/// SQLite error when a statement fails; either way nothing is committed.
pub fn stage_run(
    conn: &mut Connection,
    input: &StageAutoImproveRun,
) -> StoreResult<StagedAutoImproveRun> {
    let now = Timestamp::now().as_microsecond();
    let run_id = AutoImproveRunId::new();
    let actor_json = serde_json::to_string(&input.proposal_actor)?;
    let warnings_json = serde_json::to_string(&input.warnings_json)?;
    let rejected_json = serde_json::to_string(&input.rejected_candidates_json)?;
    let config_json = serde_json::to_string(&input.config_json)?;
    let tx = conn.transaction()?;
    if let Some(session_id) = input.session_id {
        tx.query_row(
            "SELECT 1 FROM sessions WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3",
            params![
                session_id.as_bytes(),
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
            ],
            |_| Ok(()),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidState("auto-improve session is not in proposal scope".into())
        })?;
    }
    tx.execute(
        "INSERT INTO auto_improve_runs \
         (id, workspace_id, project_id, session_id, provider, model, summary, warnings_json, \
          rejected_candidates_json, config_json, proposal_actor_json, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            run_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
            input.session_id.map(|id| id.as_bytes().to_vec()),
            input.provider.as_deref(),
            input.model.as_deref(),
            input.summary.as_deref(),
            warnings_json,
            rejected_json,
            config_json,
            actor_json,
            now,
        ],
    )?;
    insert_rejected_candidates_in_tx(&tx, input, run_id, now)?;
    let mut proposal_ids = Vec::with_capacity(input.proposals.len());
    let mut skipped: Vec<SkippedProposal> = Vec::new();
    // Two proposals in ONE run targeting the same path mean the batch itself is
    // malformed — the model emitted contradictory suggestions for one page, and
    // silently keeping an arbitrary one would be worse than refusing. That
    // stays a hard error (see `auto_improve_duplicate_pending_target_rolls_back_run`).
    // A collision with a proposal already PENDING from an earlier run is a
    // different situation, handled per-proposal below.
    let mut seen_targets: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for proposal in &input.proposals {
        if !seen_targets.insert(proposal.target_path.as_str()) {
            return Err(StoreError::InvalidState(format!(
                "two proposals in the same run target {}",
                proposal.target_path
            )));
        }
        let id = AutoImproveProposalId::new();
        let artifact_path = artifact_path_for(id);
        let evidence_json = serde_json::to_string(&proposal.evidence_json)?;
        let body_sha256 = sha256(proposal.body_markdown.as_bytes());
        let edit_mode = proposal.edit_mode.as_deref().unwrap_or("full_page");
        let patch_json = proposal
            .patch_json
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?;
        let target_snapshot = latest_target_snapshot(
            &tx,
            input.workspace_id,
            input.project_id,
            proposal.target_path.as_str(),
        )?;
        let (
            target_latest_page_id_at_stage,
            target_body_sha256_at_stage,
            target_updated_at_at_stage,
        ) = match (proposal.operation, target_snapshot) {
            (AutoImproveProposalOperation::Create, None) => (None, None, None),
            (AutoImproveProposalOperation::Create, Some(_)) => {
                return Err(StoreError::InvalidState(format!(
                    "create proposal target already exists: {}",
                    proposal.target_path
                )));
            }
            (AutoImproveProposalOperation::Update, Some(snapshot)) => (
                Some(snapshot.page_id),
                Some(bytes32(snapshot.body_sha256)?),
                Some(snapshot.updated_at),
            ),
            (AutoImproveProposalOperation::Update, None) => {
                return Err(StoreError::InvalidState(format!(
                    "update proposal target does not exist: {}",
                    proposal.target_path
                )));
            }
        };
        if edit_mode == "patch" {
            if proposal.operation != AutoImproveProposalOperation::Update {
                return Err(StoreError::InvalidState(format!(
                    "patch proposal must use update operation: {}",
                    proposal.target_path
                )));
            }
            if patch_json.is_none() {
                return Err(StoreError::InvalidState(format!(
                    "patch proposal missing patch_json: {}",
                    proposal.target_path
                )));
            }
            let Some(expected) = proposal.expected_base_body_sha256 else {
                return Err(StoreError::InvalidState(format!(
                    "patch proposal missing expected base body hash: {}",
                    proposal.target_path
                )));
            };
            match target_body_sha256_at_stage {
                Some(current) if current == expected => {}
                Some(_) => {
                    return Err(StoreError::InvalidState(format!(
                        "auto-improve proposal target changed since patch materialization: {}",
                        proposal.target_path
                    )));
                }
                None => {
                    return Err(StoreError::InvalidState(format!(
                        "patch proposal target does not exist: {}",
                        proposal.target_path
                    )));
                }
            }
        }
        let insert = tx.execute(
            "INSERT INTO auto_improve_proposals \
             (id, run_id, workspace_id, project_id, status, operation, target_path, kind, title, \
              confidence, rationale, evidence_json, body_markdown, body_sha256, artifact_path, \
              artifact_sha256, target_latest_page_id_at_stage, target_body_sha256_at_stage, \
              target_updated_at_at_stage, staged_at, edit_mode, patch_json, \
              expected_base_body_sha256, materialized_base_body_sha256, \
              staged_by_actor_user) \
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, \
                     ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
            params![
                id.as_bytes(),
                run_id.as_bytes(),
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
                proposal.operation.as_str(),
                proposal.target_path.as_str(),
                proposal.kind.as_str(),
                proposal.title.as_str(),
                proposal.confidence,
                proposal.rationale.as_str(),
                evidence_json,
                proposal.body_markdown.as_str(),
                body_sha256.as_slice(),
                artifact_path,
                proposal.artifact_sha256.map(|h| h.to_vec()),
                target_latest_page_id_at_stage.map(|id| id.as_bytes().to_vec()),
                target_body_sha256_at_stage.map(|h| h.to_vec()),
                target_updated_at_at_stage,
                now,
                edit_mode,
                patch_json,
                proposal.expected_base_body_sha256.map(|h| h.to_vec()),
                proposal.expected_base_body_sha256.map(|h| h.to_vec()),
                input.staged_by_actor_user.as_deref(),
            ],
        );
        // A pending proposal already exists for this target. Skip just this
        // one: aborting here would discard the run row and every sibling
        // proposal, which is a strictly worse outcome than not staging one
        // duplicate. Same tolerated-constraint shape as `get_or_create_project`.
        match insert {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(err, _)) if is_pending_target_collision(&err) => {
                skipped.push(SkippedProposal {
                    target_path: proposal.target_path.to_string(),
                    reason: "a proposal is already pending review for this path".into(),
                });
                continue;
            }
            Err(e) => return Err(e.into()),
        }
        insert_event_in_tx(
            &tx,
            id,
            "staged",
            &input.proposal_actor,
            None,
            &serde_json::json!({}),
            now,
        )?;
        proposal_ids.push(id);
    }
    tx.commit()?;
    Ok(StagedAutoImproveRun {
        run_id,
        proposal_ids,
        skipped,
    })
}

/// Mark a pending proposal `failed`, record the rejection fingerprint, and
/// append a `failed` event.
///
/// # Errors
/// Returns [`StoreError::InvalidState`] when the proposal is not pending in
/// the given scope, or an SQLite error when a statement fails.
pub fn fail_proposal(conn: &mut Connection, input: &FailAutoImproveProposal) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let actor_json = serde_json::to_string(&input.actor)?;
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE auto_improve_proposals \
         SET status = 'failed', decided_at = ?1, decision_reason = ?2, \
             decided_by_author_id = ?3, decided_by_actor_json = ?4 \
         WHERE id = ?5 AND workspace_id = ?6 AND project_id = ?7 AND status = 'pending'",
        params![
            now,
            input.reason.as_str(),
            input.author_id.map(|id| id.as_bytes().to_vec()),
            actor_json,
            input.proposal_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidState(
            "auto-improve proposal is not pending or not in scope".into(),
        ));
    }
    insert_rejection_for_proposal_in_tx(&tx, input.proposal_id, &input.reason, now)?;
    insert_event_in_tx(
        &tx,
        input.proposal_id,
        "failed",
        &input.actor,
        input.author_id,
        &serde_json::json!({ "reason": input.reason.as_str() }),
        now,
    )?;
    tx.commit()?;
    Ok(())
}

/// Mark a pending proposal `rejected`, record the rejection fingerprint, and
/// append a `rejected` event.
///
/// # Errors
/// Returns [`StoreError::InvalidState`] when the proposal is not pending in
/// the given scope, or an SQLite error when a statement fails.
pub fn reject_proposal(
    conn: &mut Connection,
    input: &RejectAutoImproveProposal,
) -> StoreResult<()> {
    let now = Timestamp::now().as_microsecond();
    let actor_json = serde_json::to_string(&input.actor)?;
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE auto_improve_proposals \
         SET status = 'rejected', decided_at = ?1, decision_reason = ?2, \
             decided_by_author_id = ?3, decided_by_actor_json = ?4 \
         WHERE id = ?5 AND workspace_id = ?6 AND project_id = ?7 AND status = 'pending'",
        params![
            now,
            input.reason.as_str(),
            input.author_id.map(|id| id.as_bytes().to_vec()),
            actor_json,
            input.proposal_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidState(
            "auto-improve proposal is not pending or not in scope".into(),
        ));
    }
    insert_rejection_for_proposal_in_tx(&tx, input.proposal_id, &input.reason, now)?;
    insert_event_in_tx(
        &tx,
        input.proposal_id,
        "rejected",
        &input.actor,
        input.author_id,
        &serde_json::json!({ "reason": input.reason.as_str() }),
        now,
    )?;
    tx.commit()?;
    Ok(())
}

/// Approve a pending proposal: re-check the stage-time target snapshot and,
/// when it still matches, write the page and mark the proposal `approved`
/// in the same transaction. A drifted target marks the proposal `conflict`
/// instead and writes nothing.
///
/// # Errors
/// Returns [`StoreError::InvalidState`] when the approval page's scope,
/// author, or path disagrees with the proposal, or when the proposal is not
/// pending in the given scope; or an SQLite error when a statement fails.
pub fn approve_proposal(
    conn: &mut Connection,
    input: &ApproveAutoImproveProposal,
) -> StoreResult<ApproveAutoImproveProposalResult> {
    if input.page.workspace_id != input.workspace_id || input.page.project_id != input.project_id {
        return Err(StoreError::InvalidState(
            "approval page scope does not match proposal scope".into(),
        ));
    }
    if input.page.author_id != input.author_id {
        return Err(StoreError::InvalidState(
            "approval page author does not match approver author".into(),
        ));
    }
    let now = Timestamp::now().as_microsecond();
    let tx = conn.transaction()?;
    let proposal = tx
        .query_row(
            "SELECT operation, target_path, target_latest_page_id_at_stage, \
                    target_body_sha256_at_stage, target_updated_at_at_stage \
             FROM auto_improve_proposals \
             WHERE id = ?1 AND workspace_id = ?2 AND project_id = ?3 AND status = 'pending'",
            params![
                input.proposal_id.as_bytes(),
                input.workspace_id.as_bytes(),
                input.project_id.as_bytes(),
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((operation, target_path, staged_page_id, staged_body_hash, staged_updated_at)) =
        proposal
    else {
        return Err(StoreError::InvalidState(
            "auto-improve proposal is not pending or not in scope".into(),
        ));
    };
    if input.page.path.as_str() != target_path {
        return Err(StoreError::InvalidState(
            "approval page path does not match proposal target".into(),
        ));
    }

    let current = latest_target_snapshot(&tx, input.workspace_id, input.project_id, &target_path)?;
    // Hard enforcement of the documented safety invariant: pinned pages are
    // never rewritten by the auto-improvement path (issue #157). The check
    // lives HERE — the single point every apply flows through, manual
    // approval and require_approval=false auto-apply alike — so no index
    // window, prompt phrasing, or approval policy can bypass it. Unpinning
    // the page first is the explicit way to allow the rewrite.
    if current.as_ref().is_some_and(|snapshot| {
        pinned_refusal_applies(&target_path, snapshot.pinned, &snapshot.frontmatter_json)
    }) {
        const REASON: &str =
            "target page is pinned; pinned pages are never rewritten by auto-improvement";
        insert_rejection_for_proposal_in_tx(&tx, input.proposal_id, REASON, now)?;
        mark_decision_in_tx(&tx, input, "conflict", None, Some(REASON), now)?;
        insert_event_in_tx(
            &tx,
            input.proposal_id,
            "conflict",
            &input.actor,
            input.author_id,
            &serde_json::json!({ "reason": REASON }),
            now,
        )?;
        tx.commit()?;
        return Ok(ApproveAutoImproveProposalResult::Conflict);
    }
    let conflict = match AutoImproveProposalOperation::from_str(&operation)? {
        AutoImproveProposalOperation::Create => current.is_some(),
        AutoImproveProposalOperation::Update => match current {
            Some(snapshot) => {
                Some(snapshot.page_id.as_bytes().to_vec()) != staged_page_id
                    || Some(snapshot.body_sha256) != staged_body_hash
                    || Some(snapshot.updated_at) != staged_updated_at
            }
            None => true,
        },
    };
    if conflict {
        insert_rejection_for_proposal_in_tx(
            &tx,
            input.proposal_id,
            "target changed since proposal was staged",
            now,
        )?;
        mark_decision_in_tx(
            &tx,
            input,
            "conflict",
            None,
            Some("target changed since proposal was staged"),
            now,
        )?;
        insert_event_in_tx(
            &tx,
            input.proposal_id,
            "conflict",
            &input.actor,
            input.author_id,
            &serde_json::json!({ "reason": "target changed since proposal was staged" }),
            now,
        )?;
        tx.commit()?;
        return Ok(ApproveAutoImproveProposalResult::Conflict);
    }

    let page_id = ops::upsert_page_in_tx(&tx, &input.page, now)?;
    mark_decision_in_tx(&tx, input, "approved", Some(page_id), None, now)?;
    insert_event_in_tx(
        &tx,
        input.proposal_id,
        "approved",
        &input.actor,
        input.author_id,
        &serde_json::json!({ "applied_page_id": page_id.to_string() }),
        now,
    )?;
    tx.commit()?;
    Ok(ApproveAutoImproveProposalResult::Approved { page_id })
}

fn mark_decision_in_tx(
    tx: &rusqlite::Transaction<'_>,
    input: &ApproveAutoImproveProposal,
    status: &str,
    applied_page_id: Option<PageId>,
    reason: Option<&str>,
    now: i64,
) -> StoreResult<()> {
    let actor_json = serde_json::to_string(&input.actor)?;
    tx.execute(
        "UPDATE auto_improve_proposals \
         SET status = ?1, decided_at = ?2, decision_reason = ?3, decided_by_author_id = ?4, \
             decided_by_actor_json = ?5, applied_page_id = ?6, checkpoint = ?7 \
         WHERE id = ?8 AND workspace_id = ?9 AND project_id = ?10 AND status = 'pending'",
        params![
            status,
            now,
            reason,
            input.author_id.map(|id| id.as_bytes().to_vec()),
            actor_json,
            applied_page_id.map(|id| id.as_bytes().to_vec()),
            input.checkpoint.as_deref(),
            input.proposal_id.as_bytes(),
            input.workspace_id.as_bytes(),
            input.project_id.as_bytes(),
        ],
    )?;
    Ok(())
}

/// The latest version of a proposal's target page at decision time.
struct TargetSnapshot {
    page_id: PageId,
    body_sha256: Vec<u8>,
    updated_at: i64,
    pinned: bool,
    frontmatter_json: String,
}

fn latest_target_snapshot(
    tx: &rusqlite::Transaction<'_>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    target_path: &str,
) -> StoreResult<Option<TargetSnapshot>> {
    let row = tx
        .query_row(
            "SELECT id, body_sha256, updated_at, pinned, frontmatter_json FROM pages \
             WHERE workspace_id = ?1 AND project_id = ?2 AND path = ?3 AND is_latest = 1",
            params![workspace_id.as_bytes(), project_id.as_bytes(), target_path],
            |row| {
                Ok(TargetSnapshot {
                    page_id: PageId::from_slice(&row.get::<_, Vec<u8>>(0)?).map_err(to_sql_err)?,
                    body_sha256: row.get::<_, Vec<u8>>(1)?,
                    updated_at: row.get::<_, i64>(2)?,
                    pinned: row.get::<_, bool>(3)?,
                    frontmatter_json: row.get::<_, String>(4)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

/// Whether the pinned-target refusal applies (issue #157). Pinned pages
/// are never rewritten by auto-improvement — with one sanctioned
/// exception: NON-invariant memory slots under `_slots/` (e.g.
/// `current-focus`, a state slot) are always pinned by the slot regime
/// and are exactly the pages auto-improvement is SUPPOSED to refresh.
/// Slots whose frontmatter declares `slot_kind: "invariant"` stay
/// protected, matching the documented safety invariant ("never rewrite
/// pinned pages or invariant slots").
fn pinned_refusal_applies(target_path: &str, pinned: bool, frontmatter_json: &str) -> bool {
    if !pinned {
        return false;
    }
    if !target_path.starts_with("_slots/") {
        return true;
    }
    serde_json::from_str::<serde_json::Value>(frontmatter_json)
        .ok()
        .and_then(|fm| {
            fm.get("slot_kind")
                .and_then(serde_json::Value::as_str)
                .map(|kind| kind.eq_ignore_ascii_case("invariant"))
        })
        .unwrap_or(false)
}

fn insert_event_in_tx(
    tx: &rusqlite::Transaction<'_>,
    proposal_id: AutoImproveProposalId,
    event: &str,
    actor: &ActorContext,
    author_id: Option<UserId>,
    detail: &serde_json::Value,
    at: i64,
) -> StoreResult<()> {
    tx.execute(
        "INSERT INTO auto_improve_proposal_events \
         (proposal_id, event, actor_json, author_id, detail_json, at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            proposal_id.as_bytes(),
            event,
            serde_json::to_string(actor)?,
            author_id.map(|id| id.as_bytes().to_vec()),
            serde_json::to_string(detail)?,
            at,
        ],
    )?;
    Ok(())
}

fn insert_rejected_candidates_in_tx(
    tx: &rusqlite::Transaction<'_>,
    input: &StageAutoImproveRun,
    run_id: AutoImproveRunId,
    now: i64,
) -> StoreResult<()> {
    let Some(candidates) = input.rejected_candidates_json.as_array() else {
        return Ok(());
    };
    for candidate in candidates {
        let reason = candidate
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .trim();
        if reason.is_empty() {
            continue;
        }
        let target_path = string_field(candidate, "target_path")
            .or_else(|| string_field(candidate, "path"))
            .or_else(|| {
                string_field(candidate, "evidence").filter(|value| PagePath::new(value).is_ok())
            });
        let summary = string_field(candidate, "summary")
            .or_else(|| string_field(candidate, "evidence"))
            .unwrap_or_else(|| reason.to_string());
        let record = NewAutoImproveRejectionRecord {
            workspace_id: input.workspace_id,
            project_id: input.project_id,
            target_path,
            kind: string_field(candidate, "kind"),
            operation: string_field(candidate, "operation"),
            edit_mode: string_field(candidate, "edit_mode"),
            reason: reason.to_string(),
            summary,
            evidence_json: candidate.clone(),
            source_run_id: Some(run_id),
            source_proposal_id: None,
        };
        insert_rejection_record_in_tx(tx, &record, now)?;
    }
    Ok(())
}

fn string_field(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

struct NewAutoImproveRejectionRecord {
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    target_path: Option<String>,
    kind: Option<String>,
    operation: Option<String>,
    edit_mode: Option<String>,
    reason: String,
    summary: String,
    evidence_json: serde_json::Value,
    source_run_id: Option<AutoImproveRunId>,
    source_proposal_id: Option<AutoImproveProposalId>,
}

fn insert_rejection_for_proposal_in_tx(
    tx: &rusqlite::Transaction<'_>,
    proposal_id: AutoImproveProposalId,
    reason: &str,
    now: i64,
) -> StoreResult<()> {
    let record = tx.query_row(
        "SELECT run_id, workspace_id, project_id, operation, target_path, kind, title, rationale, \
                evidence_json, edit_mode \
         FROM auto_improve_proposals WHERE id = ?1",
        params![proposal_id.as_bytes()],
        |row| {
            let run_id =
                AutoImproveRunId::from_slice(&row.get::<_, Vec<u8>>(0)?).map_err(to_sql_err)?;
            let workspace_id =
                WorkspaceId::from_slice(&row.get::<_, Vec<u8>>(1)?).map_err(to_sql_err)?;
            let project_id =
                ProjectId::from_slice(&row.get::<_, Vec<u8>>(2)?).map_err(to_sql_err)?;
            let evidence_raw: String = row.get(8)?;
            let title: String = row.get(6)?;
            let rationale: String = row.get(7)?;
            Ok(NewAutoImproveRejectionRecord {
                workspace_id,
                project_id,
                target_path: Some(row.get(4)?),
                kind: Some(row.get(5)?),
                operation: Some(row.get(3)?),
                edit_mode: Some(row.get(9)?),
                reason: reason.to_string(),
                summary: if title.trim().is_empty() {
                    rationale
                } else {
                    title
                },
                evidence_json: serde_json::from_str(&evidence_raw).map_err(to_sql_err)?,
                source_run_id: Some(run_id),
                source_proposal_id: Some(proposal_id),
            })
        },
    )?;
    insert_rejection_record_in_tx(tx, &record, now)
}

fn insert_rejection_record_in_tx(
    tx: &rusqlite::Transaction<'_>,
    record: &NewAutoImproveRejectionRecord,
    now: i64,
) -> StoreResult<()> {
    let id = Uuid::new_v4();
    let evidence_json = serde_json::to_string(&record.evidence_json)?;
    let fingerprint = rejection_fingerprint(record);
    tx.execute(
        "INSERT INTO auto_improve_rejections \
         (id, workspace_id, project_id, target_path, kind, operation, edit_mode, reason, \
          normalized_fingerprint, summary, evidence_json, source_run_id, source_proposal_id, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id.as_bytes().as_slice(),
            record.workspace_id.as_bytes(),
            record.project_id.as_bytes(),
            record.target_path.as_deref(),
            record.kind.as_deref(),
            record.operation.as_deref(),
            record.edit_mode.as_deref(),
            record.reason.as_str(),
            fingerprint,
            record.summary.as_str(),
            evidence_json,
            record.source_run_id.map(|id| id.as_bytes().to_vec()),
            record.source_proposal_id.map(|id| id.as_bytes().to_vec()),
            now,
        ],
    )?;
    Ok(())
}

fn rejection_fingerprint(record: &NewAutoImproveRejectionRecord) -> String {
    let input = [
        normalize_fp(record.target_path.as_deref().unwrap_or("")),
        normalize_fp(record.kind.as_deref().unwrap_or("")),
        normalize_fp(record.operation.as_deref().unwrap_or("")),
        normalize_fp(record.edit_mode.as_deref().unwrap_or("")),
        normalize_fp(&record.reason),
        normalize_fp(&record.summary),
    ]
    .join("\n");
    hex_sha256(input.as_bytes())
}

fn normalize_fp(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn hex_sha256(bytes: &[u8]) -> String {
    let hash = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in hash {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

/// Is this insert failure the one-pending-proposal-per-target collision that
/// [`stage_run`] is allowed to swallow?
///
/// Only the UNIQUE extended code qualifies. `ErrorCode::ConstraintViolation` is
/// the PRIMARY code shared by NOT NULL, CHECK, FK and `RAISE(ABORT)` failures,
/// and `auto_improve_proposals` has CHECKs on `status`/`operation`, FKs to four
/// tables, and a workspace/project pairing trigger — accepting the primary code
/// would relabel any of those "a proposal is already pending review for this
/// path" and drop the proposal instead of surfacing a schema-contract bug.
fn is_pending_target_collision(err: &rusqlite::ffi::Error) -> bool {
    err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

pub(crate) fn summary_from_row(row: &Row<'_>) -> rusqlite::Result<AutoImproveProposalSummary> {
    let status: String = row.get(4)?;
    let operation: String = row.get(5)?;
    let id = AutoImproveProposalId::from_slice(&row.get::<_, Vec<u8>>(0)?).map_err(to_sql_err)?;
    let run_id = AutoImproveRunId::from_slice(&row.get::<_, Vec<u8>>(1)?).map_err(to_sql_err)?;
    let workspace_id = WorkspaceId::from_slice(&row.get::<_, Vec<u8>>(2)?).map_err(to_sql_err)?;
    let project_id = ProjectId::from_slice(&row.get::<_, Vec<u8>>(3)?).map_err(to_sql_err)?;
    let target_path = PagePath::new(row.get::<_, String>(6)?).map_err(to_sql_err)?;
    Ok(AutoImproveProposalSummary {
        id,
        run_id,
        workspace_id,
        project_id,
        status: AutoImproveProposalStatus::from_str(&status).map_err(to_sql_err)?,
        operation: AutoImproveProposalOperation::from_str(&operation).map_err(to_sql_err)?,
        target_path,
        kind: row.get(7)?,
        title: row.get(8)?,
        confidence: row.get(9)?,
        staged_at: row.get(10)?,
        decided_at: row.get(11)?,
    })
}

pub(crate) fn bytes32(bytes: Vec<u8>) -> StoreResult<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| StoreError::MalformedRecord("invalid sha256 length".into()))
}

pub(crate) fn opt_bytes32(bytes: Option<Vec<u8>>) -> StoreResult<Option<[u8; 32]>> {
    bytes.map(bytes32).transpose()
}

pub(crate) fn to_sql_err<E: std::error::Error + Send + Sync + 'static>(err: E) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Blob, Box::new(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Issue #157: pinned pages are refused; the sanctioned exception is
    // NON-invariant `_slots/` pages (the slot regime pins everything, and
    // state slots like current-focus are exactly what auto-improvement is
    // supposed to refresh). Invariant slots stay protected.
    #[test]
    fn pinned_refusal_spares_state_slots_but_not_invariant_slots() {
        // Regular pinned page: refused.
        assert!(pinned_refusal_applies("decisions/adr-0001.md", true, "{}"));
        // Unpinned: never refused.
        assert!(!pinned_refusal_applies(
            "decisions/adr-0001.md",
            false,
            "{}"
        ));
        // State slot (default kind): allowed.
        assert!(!pinned_refusal_applies(
            "_slots/current-focus.md",
            true,
            "{}"
        ));
        assert!(!pinned_refusal_applies(
            "_slots/current-focus.md",
            true,
            r#"{"slot_kind": "state"}"#
        ));
        // Invariant slot: refused.
        assert!(pinned_refusal_applies(
            "_slots/never-force-push.md",
            true,
            r#"{"slot_kind": "invariant"}"#
        ));
        // Malformed frontmatter on a slot: treated as state (allowed) —
        // the slot regime owns those pages either way.
        assert!(!pinned_refusal_applies("_slots/x.md", true, "not-json"));
    }
}
