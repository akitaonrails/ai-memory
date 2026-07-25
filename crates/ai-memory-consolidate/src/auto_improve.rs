//! Reviewer for optional auto-improvement proposals.
//!
//! This module is intentionally read-only. It inspects one completed session,
//! asks the configured LLM for structured wiki edit proposals, validates those
//! proposals, and returns a report. CLI/admin code may stage the validated
//! proposals after this read-only review returns.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use ai_memory_core::{Observation, PagePath, ProjectId, SessionId, WorkspaceId};
use ai_memory_llm::{ChatMessage, ChatRequest, LlmError, LlmProvider, Role, complete_structured};
use ai_memory_store::{AutoImproveRejectionSummary, BriefingPage, ReaderPool, StoredPageBody};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::projection::{ObservationProjectionConfig, cap_text_with_marker, project_observations};

const CHARS_PER_TOKEN: usize = 4;
const PROMPT_RESERVE_TOKENS: usize = 1_000;
const MAX_PROPOSAL_BODY_CHARS: usize = 32_000;
/// Default number of existing pages included as patchable context.
pub const DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_PAGES: usize = 8;
/// Default maximum body chars rendered for one patchable target page.
pub const DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_BODY_CHARS: usize = 8_000;
/// Default maximum patch edits per proposal.
pub const DEFAULT_AUTO_IMPROVE_MAX_EDITS_PER_PROPOSAL: usize = 5;
/// Default maximum content chars in one patch edit.
pub const DEFAULT_AUTO_IMPROVE_MAX_EDIT_CONTENT_CHARS: usize = 4_000;
/// Default maximum aggregate changed chars in one patch proposal.
pub const DEFAULT_AUTO_IMPROVE_MAX_CHANGED_CHARS_PER_PROPOSAL: usize = 12_000;
/// Default maximum patch edits accepted across one review run.
pub const DEFAULT_AUTO_IMPROVE_MAX_PATCH_EDITS_PER_RUN: usize = 8;
/// Default maximum rejection-buffer entries rendered into one review prompt.
pub const DEFAULT_AUTO_IMPROVE_MAX_REJECTION_CONTEXT: usize = 50;
/// Default maximum age for rejection-buffer prompt context.
pub const DEFAULT_AUTO_IMPROVE_REJECTION_CONTEXT_DAYS: u32 = 180;
/// Default maximum materialized final body size.
pub const DEFAULT_AUTO_IMPROVE_MAX_FINAL_BODY_CHARS: usize = MAX_PROPOSAL_BODY_CHARS;
/// Default approximate token budget for one rule page.
pub const DEFAULT_AUTO_IMPROVE_MAX_RULE_PAGE_TOKENS: usize = 2_000;
/// Default approximate token budget for one procedure page.
pub const DEFAULT_AUTO_IMPROVE_MAX_PROCEDURE_PAGE_TOKENS: usize = 2_000;
const DEFAULT_REVIEW_MAX_TOKENS: u32 = 16_000;
const MAX_SESSION_PAGE_CHARS: usize = 32_000;
const SAMPLE_LIMIT_WITH_SESSION_PAGE: usize = 48;
const SAMPLE_LIMIT_WITHOUT_SESSION_PAGE: usize = 72;
// Per-observation body cap in the reviewer projection. Note this is a SECOND,
// tighter truncation than storage: the opt-in assistant/Stop excerpt (#196) is
// persisted at up to 2 KB, but the reviewer only ever sees its first 1500 chars
// here (head-biased). A late correction inside a long Stop body must fall within
// this window to influence the reviewer.
const MAX_OBSERVATION_BODY_CHARS: usize = 1_500;
const PROMPT_SCAFFOLD_RESERVE_CHARS: usize = 4_000;
const MAX_REJECTION_CONTEXT_CHARS: usize = 12_000;
const MAX_REJECTION_PATH_CHARS: usize = 256;
const MAX_REJECTION_REASON_CHARS: usize = 512;
const MAX_REJECTION_FINGERPRINT_CHARS: usize = 128;
const MAX_REJECTION_SUMMARY_CHARS: usize = 1_024;
const MAX_EVAL_STDOUT_BYTES: usize = 64 * 1024;
const MAX_EVAL_REASON_CHARS: usize = 2_048;

/// Default confidence floor for staged auto-improvement proposals.
pub const DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE: f32 = 0.75;
/// Default minimum observation count before a session is reviewed.
pub const DEFAULT_AUTO_IMPROVE_MIN_OBSERVATIONS: usize = 8;
/// Default minimum session duration before a session is reviewed.
pub const DEFAULT_AUTO_IMPROVE_MIN_SESSION_DURATION_SECS: u64 = 120;
/// Default approximate input token budget for one review.
pub const DEFAULT_AUTO_IMPROVE_MAX_INPUT_TOKENS: usize = 24_000;
/// Default maximum validated proposals per review.
pub const DEFAULT_AUTO_IMPROVE_MAX_PROPOSALS: usize = 5;
/// Default synthetic actor name for autonomous proposal provenance.
pub const DEFAULT_AUTO_IMPROVE_PROPOSAL_ACTOR: &str = "auto_improve";
/// Default wiki-relative folder for pending proposal sidecar markdown.
pub const DEFAULT_AUTO_IMPROVE_PENDING_PATH: &str = "_pending/auto-improve";

/// Default target prefixes guarded by the optional external eval command.
pub fn default_auto_improve_eval_targets() -> Vec<String> {
    vec!["_rules".into(), "procedures".into()]
}

/// Optional executable gate for validated auto-improvement proposals.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoImproveEvalConfig {
    /// Whether the external eval gate is enabled.
    pub enabled: bool,
    /// Executable command plus whitespace-separated args. Executed directly, not via a shell.
    pub command: String,
    /// Timeout for one proposal evaluation.
    pub timeout_secs: u64,
    /// Wiki path prefixes that require eval when enabled.
    pub targets: Vec<String>,
    /// Required score_after - score_before when scores are returned.
    pub min_delta: f64,
}

impl Default for AutoImproveEvalConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: String::new(),
            timeout_secs: 120,
            targets: default_auto_improve_eval_targets(),
            min_delta: 0.0,
        }
    }
}

/// Configuration for one auto-improvement review.
#[derive(Debug, Clone)]
pub struct AutoImproveReviewConfig {
    /// Minimum observations before a session is worth reviewing.
    pub min_observations: usize,
    /// Minimum session duration before a session is worth reviewing.
    pub min_session_duration_secs: u64,
    /// Minimum model confidence accepted by validation.
    pub min_confidence: f32,
    /// Approximate input token budget, using chars/4.
    pub max_input_tokens: usize,
    /// Maximum validated proposals returned from one run.
    pub max_proposals_per_run: usize,
    /// Whether raw fallback content may be considered. Hooks still provide raw observations.
    pub include_raw_fallback: bool,
    /// Synthetic actor name used for staged proposal provenance.
    pub proposal_actor: String,
    /// Wiki-relative pending proposal sidecar folder.
    pub pending_path: String,
    /// Maximum existing _rules/ and procedures/ pages included for patch proposals.
    pub max_patchable_pages: usize,
    /// Maximum body chars rendered per patchable target page.
    pub max_patchable_body_chars: usize,
    /// Maximum patch edits per proposal.
    pub max_edits_per_proposal: usize,
    /// Maximum content chars in one patch edit.
    pub max_edit_content_chars: usize,
    /// Maximum aggregate changed chars in one patch proposal.
    pub max_changed_chars_per_proposal: usize,
    /// Maximum patch edits accepted across one review run.
    pub max_patch_edits_per_run: usize,
    /// Maximum recent rejected attempts rendered into prompt context.
    pub max_rejection_context: usize,
    /// Maximum age in days for rejected attempts rendered into prompt context.
    pub rejection_context_days: u32,
    /// Maximum materialized final body size.
    pub max_final_body_chars: usize,
    /// Maximum approximate tokens allowed in a final _rules/ page.
    pub max_rule_page_tokens: usize,
    /// Maximum approximate tokens allowed in a final procedures/ page.
    pub max_procedure_page_tokens: usize,
    /// Optional executable eval gate settings.
    pub eval: AutoImproveEvalConfig,
}

impl Default for AutoImproveReviewConfig {
    fn default() -> Self {
        Self {
            min_observations: DEFAULT_AUTO_IMPROVE_MIN_OBSERVATIONS,
            min_session_duration_secs: DEFAULT_AUTO_IMPROVE_MIN_SESSION_DURATION_SECS,
            min_confidence: DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE,
            max_input_tokens: DEFAULT_AUTO_IMPROVE_MAX_INPUT_TOKENS,
            max_proposals_per_run: DEFAULT_AUTO_IMPROVE_MAX_PROPOSALS,
            include_raw_fallback: false,
            proposal_actor: DEFAULT_AUTO_IMPROVE_PROPOSAL_ACTOR.into(),
            pending_path: DEFAULT_AUTO_IMPROVE_PENDING_PATH.into(),
            max_patchable_pages: DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_PAGES,
            max_patchable_body_chars: DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_BODY_CHARS,
            max_edits_per_proposal: DEFAULT_AUTO_IMPROVE_MAX_EDITS_PER_PROPOSAL,
            max_edit_content_chars: DEFAULT_AUTO_IMPROVE_MAX_EDIT_CONTENT_CHARS,
            max_changed_chars_per_proposal: DEFAULT_AUTO_IMPROVE_MAX_CHANGED_CHARS_PER_PROPOSAL,
            max_patch_edits_per_run: DEFAULT_AUTO_IMPROVE_MAX_PATCH_EDITS_PER_RUN,
            max_rejection_context: DEFAULT_AUTO_IMPROVE_MAX_REJECTION_CONTEXT,
            rejection_context_days: DEFAULT_AUTO_IMPROVE_REJECTION_CONTEXT_DAYS,
            max_final_body_chars: DEFAULT_AUTO_IMPROVE_MAX_FINAL_BODY_CHARS,
            max_rule_page_tokens: DEFAULT_AUTO_IMPROVE_MAX_RULE_PAGE_TOKENS,
            max_procedure_page_tokens: DEFAULT_AUTO_IMPROVE_MAX_PROCEDURE_PAGE_TOKENS,
            eval: AutoImproveEvalConfig::default(),
        }
    }
}

/// Errors raised by the reviewer.
#[derive(Debug, Error)]
pub enum AutoImproveError {
    /// Underlying store error.
    #[error(transparent)]
    Store(#[from] ai_memory_store::StoreError),
    /// Underlying LLM error.
    #[error(transparent)]
    Llm(#[from] LlmError),
    /// Domain parsing error.
    #[error(transparent)]
    Memory(#[from] ai_memory_core::MemoryError),
    /// Session was not found.
    #[error("session not found: {0}")]
    SessionNotFound(SessionId),
    /// Session belongs to a different scope than the request selected.
    #[error("session {session_id} belongs to a different workspace/project")]
    SessionOutOfScope {
        /// Session that failed the scope check.
        session_id: SessionId,
    },
    /// Optional external eval command failed unexpectedly.
    #[error("auto-improve eval gate failed: {0}")]
    Eval(String),
}

/// Result alias for auto-improvement review.
pub type AutoImproveResult<T> = Result<T, AutoImproveError>;

/// One evidence quote cited by a proposal.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AutoImproveEvidence {
    /// Source page or observation label, such as `sessions/<id>.md`.
    pub page: String,
    /// Bounded quote supporting the proposed durable edit.
    pub quote: String,
}

impl<'de> Deserialize<'de> for AutoImproveEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum EvidenceInput {
            Object {
                #[serde(default)]
                page: String,
                #[serde(default)]
                quote: String,
            },
            Quote(String),
        }

        match EvidenceInput::deserialize(deserializer)? {
            EvidenceInput::Object { page, quote } => Ok(Self {
                page: if page.trim().is_empty() && !quote.trim().is_empty() {
                    "unspecified".into()
                } else {
                    page
                },
                quote,
            }),
            EvidenceInput::Quote(quote) => Ok(Self {
                page: "unspecified".into(),
                quote,
            }),
        }
    }
}

/// One proposed wiki edit returned by the LLM and accepted by validation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutoImproveProposal {
    /// Currently only `create_or_update` is supported.
    #[serde(default = "default_operation")]
    pub operation: String,
    /// Relative wiki path that would be created or updated.
    #[serde(default)]
    pub path: String,
    /// Human title for the proposed page.
    #[serde(default)]
    pub title: String,
    /// Semantic kind: gotcha, decision, concept, procedure, rule, fact, note, or slot.
    #[serde(default)]
    pub kind: String,
    /// Model confidence from 0.0 to 1.0.
    #[serde(default)]
    pub confidence: f32,
    /// Why the lesson is durable enough to propose.
    #[serde(default)]
    pub rationale: String,
    /// Evidence quotes that justify the proposal.
    #[serde(default)]
    pub evidence: Vec<AutoImproveEvidence>,
    /// Markdown body without frontmatter.
    #[serde(default, alias = "body", alias = "markdown", alias = "content")]
    pub body_markdown: String,
    /// `full_page` (default) or `patch`.
    #[serde(default = "default_edit_mode")]
    pub edit_mode: String,
    /// Patch edits for existing _rules/ or procedures/ pages.
    #[serde(default)]
    pub edits: Vec<AutoImprovePatchEdit>,
    /// Base body sha256 computed when a patch is materialized.
    #[serde(default)]
    pub expected_base_body_sha256: Option<String>,
}

/// One anchored patch edit.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutoImprovePatchEdit {
    /// `append`, `add_section`, or `replace_section`.
    #[serde(default)]
    pub op: String,
    /// Exact markdown heading anchor, including marker, e.g. `## Release process`.
    #[serde(default)]
    pub anchor: String,
    /// Markdown content to insert or replace.
    #[serde(default)]
    pub content: String,
    /// Required for replace_section; sha256 of the anchored section span.
    #[serde(default)]
    pub section_sha256: Option<String>,
    /// Optional surrounding context for model readability.
    #[serde(default)]
    pub context: Option<String>,
}

fn default_operation() -> String {
    "create_or_update".into()
}

fn default_edit_mode() -> String {
    "full_page".into()
}

/// A candidate the reviewer or validator rejected.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutoImproveRejectedCandidate {
    /// Machine-readable-ish reason for rejection.
    #[serde(default)]
    pub reason: String,
    /// Evidence label or short detail.
    #[serde(default)]
    pub evidence: String,
    /// Optional target path for validator/model rejections.
    #[serde(default)]
    pub target_path: Option<String>,
    /// Optional semantic kind for the rejected edit.
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional requested operation.
    #[serde(default)]
    pub operation: Option<String>,
    /// Optional edit mode.
    #[serde(default)]
    pub edit_mode: Option<String>,
}

/// Structured response requested from the LLM.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutoImproveLlmResponse {
    /// Short summary of the review.
    #[serde(default)]
    pub summary: String,
    /// Candidate edits.
    #[serde(default)]
    pub proposals: Vec<AutoImproveProposal>,
    /// Candidates the model chose not to promote.
    #[serde(default)]
    pub rejected_candidates: Vec<AutoImproveRejectedCandidate>,
}

/// Report returned by the reviewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoImproveReport {
    /// Reviewed session id.
    pub session_id: String,
    /// Number of observations read from the session.
    pub observations_considered: usize,
    /// Approximate session span, first observation to last observation.
    pub session_duration_secs: u64,
    /// Approximate prompt tokens sent to the LLM, chars/4.
    pub estimated_input_tokens: usize,
    /// LLM provider name, or `none` when the preflight filter skipped the call.
    pub provider: String,
    /// LLM model, or `none` when the preflight filter skipped the call.
    pub model: String,
    /// Configured confidence floor used by validation.
    pub min_confidence: f32,
    /// Actor name used for staged proposal provenance.
    pub proposal_actor: String,
    /// Wiki-relative pending proposal sidecar path.
    pub pending_path: String,
    /// Review summary.
    pub summary: String,
    /// Validated proposals. Target wiki pages have not been written.
    pub proposals: Vec<AutoImproveProposal>,
    /// Model and validator rejections.
    pub rejected_candidates: Vec<AutoImproveRejectedCandidate>,
    /// Non-fatal validation or budget notes.
    pub warnings: Vec<String>,
}

/// Run a read-only auto-improvement review for one session.
///
/// # Errors
/// Returns store, scope, or LLM errors. This function never writes wiki files or
/// SQLite rows.
pub async fn run_auto_improve_review(
    reader: &ReaderPool,
    llm: &(dyn LlmProvider + 'static),
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    session_id: SessionId,
    cfg: AutoImproveReviewConfig,
) -> AutoImproveResult<AutoImproveReport> {
    match reader.session_project_ids(session_id).await? {
        Some((session_ws, session_proj))
            if session_ws == workspace_id && session_proj == project_id => {}
        Some(_) => return Err(AutoImproveError::SessionOutOfScope { session_id }),
        None => return Err(AutoImproveError::SessionNotFound(session_id)),
    }

    let observations = reader.observations_for_session(session_id).await?;
    let duration = session_duration_secs(&observations);
    if let Some(rejection) = preflight_rejection(&observations, duration, &cfg) {
        return Ok(AutoImproveReport {
            session_id: session_id.to_string(),
            observations_considered: observations.len(),
            session_duration_secs: duration,
            estimated_input_tokens: 0,
            provider: "none".into(),
            model: "none".into(),
            min_confidence: cfg.min_confidence,
            proposal_actor: cfg.proposal_actor,
            pending_path: cfg.pending_path,
            summary: "session skipped by preflight filters".into(),
            proposals: Vec::new(),
            rejected_candidates: vec![rejection],
            warnings: Vec::new(),
        });
    }

    let briefing = reader
        .briefing_for_project(workspace_id, project_id, 100)
        .await?;
    let session_page_path = format!("sessions/{session_id}.md");
    let session_page = reader
        .page_body_by_ids(workspace_id, project_id, &session_page_path)
        .await?;
    let patchable_pages = load_patchable_pages(
        reader,
        workspace_id,
        project_id,
        &briefing.recent_pages,
        &cfg,
    )
    .await?;
    let rejection_context = load_rejection_context(reader, workspace_id, project_id, &cfg).await?;
    let prompt_input = build_prompt_input(
        session_id,
        &observations,
        duration,
        session_page.as_ref(),
        &briefing.recent_pages,
        &patchable_pages,
        &rejection_context,
        &cfg,
    );
    let prompt_patchable_pages: Vec<_> = patchable_pages
        .iter()
        .filter(|page| prompt_input.patchable_paths.contains(&page.path))
        .cloned()
        .collect();
    let existing_index =
        ExistingPageIndex::from_pages(&briefing.recent_pages, &prompt_patchable_pages);
    let estimated_input_tokens = estimate_tokens(&prompt_input.prompt);
    let request = ChatRequest {
        system: Some(AUTO_IMPROVE_SYSTEM_PROMPT.to_string()),
        messages: vec![ChatMessage {
            role: Role::User,
            content: prompt_input.prompt,
        }],
        max_tokens: DEFAULT_REVIEW_MAX_TOKENS,
        temperature: Some(0.1),
    };
    let raw: AutoImproveLlmResponse = complete_structured(llm, request).await?;
    let (mut proposals, mut rejected_candidates, mut warnings) =
        validate_response(raw, &cfg, &existing_index);
    rejected_candidates.extend(prompt_input.rejected_candidates);
    warnings.extend(prompt_input.warnings);
    apply_eval_gate(
        reader,
        workspace_id,
        project_id,
        &cfg.eval,
        &mut proposals,
        &mut rejected_candidates,
        &mut warnings,
    )
    .await?;

    Ok(AutoImproveReport {
        session_id: session_id.to_string(),
        observations_considered: observations.len(),
        session_duration_secs: duration,
        estimated_input_tokens,
        provider: llm.name().to_string(),
        model: llm.model().to_string(),
        min_confidence: cfg.min_confidence,
        proposal_actor: cfg.proposal_actor,
        pending_path: cfg.pending_path,
        summary: if proposals.is_empty() {
            "review completed; no validated proposals".into()
        } else {
            format!(
                "review completed; {} proposal(s) validated",
                proposals.len()
            )
        },
        proposals,
        rejected_candidates,
        warnings,
    })
}

#[derive(Debug, Serialize)]
struct AutoImproveEvalRequest<'a> {
    path: &'a str,
    kind: &'a str,
    operation: &'a str,
    edit_mode: &'a str,
    title: &'a str,
    confidence: f32,
    rationale: &'a str,
    before_body: &'a str,
    after_body: &'a str,
    expected_base_body_sha256: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct AutoImproveEvalResponse {
    passed: bool,
    score_before: Option<f64>,
    score_after: Option<f64>,
    #[serde(default)]
    reason: Option<String>,
}

async fn apply_eval_gate(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    eval: &AutoImproveEvalConfig,
    proposals: &mut Vec<AutoImproveProposal>,
    rejected: &mut Vec<AutoImproveRejectedCandidate>,
    warnings: &mut Vec<String>,
) -> AutoImproveResult<()> {
    if !eval.enabled {
        return Ok(());
    }
    let mut before_bodies = BTreeMap::new();
    for proposal in proposals.iter() {
        if !eval_targets_path(eval, &proposal.path) {
            continue;
        }
        let before_body = reader
            .page_body_by_ids(workspace_id, project_id, &proposal.path)
            .await?
            .map(|page| page.body)
            .unwrap_or_default();
        before_bodies.insert(proposal.path.clone(), before_body);
    }
    apply_eval_gate_with_before_bodies(eval, proposals, rejected, warnings, &before_bodies).await;
    Ok(())
}

async fn apply_eval_gate_with_before_bodies(
    eval: &AutoImproveEvalConfig,
    proposals: &mut Vec<AutoImproveProposal>,
    rejected: &mut Vec<AutoImproveRejectedCandidate>,
    warnings: &mut Vec<String>,
    before_bodies: &BTreeMap<String, String>,
) {
    if !eval.enabled {
        return;
    }
    if eval.command.trim().is_empty() {
        warnings.push(
            "auto-improve eval gate enabled without a command; targeted proposals rejected".into(),
        );
    }

    let mut accepted = Vec::with_capacity(proposals.len());
    for mut proposal in proposals.drain(..) {
        if !eval_targets_path(eval, &proposal.path) {
            accepted.push(proposal);
            continue;
        }
        let before_body = before_bodies.get(&proposal.path).map_or("", String::as_str);
        match run_eval_for_proposal(eval, &proposal, before_body).await {
            Ok(outcome) => {
                let delta = outcome
                    .score_before
                    .zip(outcome.score_after)
                    .map(|(before, after)| after - before);
                let delta_ok = delta.is_none_or(|value| value >= eval.min_delta);
                if outcome.passed && delta_ok {
                    proposal.evidence.push(AutoImproveEvidence {
                        page: "auto_improve_eval".into(),
                        quote: format_eval_evidence(&outcome, delta),
                    });
                    accepted.push(proposal);
                } else {
                    rejected.push(eval_rejection(
                        "eval_gate_failed",
                        &proposal,
                        format_eval_failure(&outcome, delta, eval.min_delta),
                    ));
                }
            }
            Err(EvalRunError::Timeout) => rejected.push(eval_rejection(
                "eval_gate_timeout",
                &proposal,
                format!("eval command timed out after {}s", eval.timeout_secs),
            )),
            Err(EvalRunError::Error(message)) => {
                rejected.push(eval_rejection("eval_gate_error", &proposal, message))
            }
        }
    }
    *proposals = accepted;
}

fn eval_targets_path(eval: &AutoImproveEvalConfig, path: &str) -> bool {
    eval.targets.iter().any(|target| {
        let target = target.trim().trim_end_matches('/');
        !target.is_empty() && (path == target || path.starts_with(&format!("{target}/")))
    })
}

#[derive(Debug)]
enum EvalRunError {
    Timeout,
    Error(String),
}

async fn run_eval_for_proposal(
    eval: &AutoImproveEvalConfig,
    proposal: &AutoImproveProposal,
    before_body: &str,
) -> Result<AutoImproveEvalResponse, EvalRunError> {
    let mut parts = eval.command.split_whitespace();
    let Some(program) = parts.next() else {
        return Err(EvalRunError::Error("eval command is empty".into()));
    };
    let input = AutoImproveEvalRequest {
        path: &proposal.path,
        kind: &proposal.kind,
        operation: &proposal.operation,
        edit_mode: &proposal.edit_mode,
        title: &proposal.title,
        confidence: proposal.confidence,
        rationale: &proposal.rationale,
        before_body,
        after_body: &proposal.body_markdown,
        expected_base_body_sha256: proposal.expected_base_body_sha256.as_deref(),
    };
    let stdin = serde_json::to_vec(&input).map_err(|e| EvalRunError::Error(e.to_string()))?;
    let mut child = tokio::process::Command::new(program)
        .args(parts)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| EvalRunError::Error(format!("failed to start eval command: {e}")))?;
    let interaction = async move {
        let Some(mut child_stdin) = child.stdin.take() else {
            return Err(EvalRunError::Error("eval command stdin unavailable".into()));
        };
        let Some(mut child_stdout) = child.stdout.take() else {
            return Err(EvalRunError::Error(
                "eval command stdout unavailable".into(),
            ));
        };

        let mut write_done = false;
        let mut stdout_done = false;
        let mut status_done = false;
        let mut stdout = Vec::new();
        let mut status = None;
        let mut write_fut: Pin<Box<dyn Future<Output = Result<(), EvalRunError>> + Send>> =
            Box::pin(async move {
                use tokio::io::AsyncWriteExt;
                child_stdin
                    .write_all(&stdin)
                    .await
                    .map_err(|e| EvalRunError::Error(format!("failed to write eval stdin: {e}")))?;
                child_stdin
                    .shutdown()
                    .await
                    .map_err(|e| EvalRunError::Error(format!("failed to close eval stdin: {e}")))
            });
        let mut stdout_fut: Pin<Box<dyn Future<Output = Result<Vec<u8>, EvalRunError>> + Send>> =
            Box::pin(async move { read_eval_stdout_capped(&mut child_stdout).await });
        let mut wait_fut = Box::pin(child.wait());

        loop {
            tokio::select! {
                write_result = &mut write_fut, if !write_done => {
                    write_result?;
                    write_done = true;
                }
                stdout_result = &mut stdout_fut, if !stdout_done => {
                    stdout = stdout_result?;
                    stdout_done = true;
                    if status_done {
                        break;
                    }
                }
                status_result = &mut wait_fut, if !status_done => {
                    status = Some(status_result.map_err(|e| EvalRunError::Error(format!("eval command failed: {e}")))?);
                    status_done = true;
                    if stdout_done {
                        break;
                    }
                }
            }
        }
        let status =
            status.ok_or_else(|| EvalRunError::Error("eval command status unavailable".into()))?;
        Ok((status, stdout))
    };

    let (status, stdout) =
        tokio::time::timeout(Duration::from_secs(eval.timeout_secs), interaction)
            .await
            .map_err(|_| EvalRunError::Timeout)??;
    if !status.success() {
        return Err(EvalRunError::Error(format!(
            "eval command exited with status {}",
            status
        )));
    }
    serde_json::from_slice::<AutoImproveEvalResponse>(&stdout)
        .map_err(|e| EvalRunError::Error(format!("invalid eval JSON: {e}")))
}

async fn read_eval_stdout_capped<R>(reader: &mut R) -> Result<Vec<u8>, EvalRunError>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut out = Vec::new();
    let cap = u64::try_from(MAX_EVAL_STDOUT_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    reader
        .take(cap)
        .read_to_end(&mut out)
        .await
        .map_err(|e| EvalRunError::Error(format!("failed to read eval stdout: {e}")))?;
    if out.len() > MAX_EVAL_STDOUT_BYTES {
        return Err(EvalRunError::Error(format!(
            "eval stdout exceeded {} bytes",
            MAX_EVAL_STDOUT_BYTES
        )));
    }
    Ok(out)
}

fn format_eval_evidence(outcome: &AutoImproveEvalResponse, delta: Option<f64>) -> String {
    match (outcome.score_before, outcome.score_after, delta) {
        (Some(before), Some(after), Some(delta)) => {
            format!(
                "eval passed; score_before={before:.4}, score_after={after:.4}, delta={delta:.4}"
            )
        }
        _ => "eval passed".into(),
    }
}

fn format_eval_failure(
    outcome: &AutoImproveEvalResponse,
    delta: Option<f64>,
    min_delta: f64,
) -> String {
    let reason = cap_text_with_marker(
        outcome.reason.as_deref().unwrap_or("eval did not pass"),
        MAX_EVAL_REASON_CHARS,
        "eval reason",
    );
    match (
        outcome.passed,
        outcome.score_before,
        outcome.score_after,
        delta,
    ) {
        (passed, Some(before), Some(after), Some(delta)) => format!(
            "passed={passed}; score_before={before:.4}; score_after={after:.4}; delta={delta:.4}; min_delta={min_delta:.4}; {reason}"
        ),
        (passed, _, _, _) => format!("passed={passed}; {reason}"),
    }
}

fn eval_rejection(
    reason: &str,
    proposal: &AutoImproveProposal,
    evidence: String,
) -> AutoImproveRejectedCandidate {
    AutoImproveRejectedCandidate {
        reason: reason.into(),
        evidence: cap_text_with_marker(
            &evidence,
            MAX_REJECTION_SUMMARY_CHARS,
            "eval rejection evidence",
        ),
        target_path: Some(proposal.path.clone()),
        kind: Some(proposal.kind.clone()),
        operation: Some(proposal.operation.clone()),
        edit_mode: Some(proposal.edit_mode.clone()),
    }
}

struct PromptInput {
    prompt: String,
    patchable_paths: BTreeSet<String>,
    rejected_candidates: Vec<AutoImproveRejectedCandidate>,
    warnings: Vec<String>,
}

#[derive(Debug, Default)]
struct RenderedPatchablePages {
    text: String,
    included_paths: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct ExistingPageIndex {
    paths: BTreeSet<String>,
    titles: BTreeSet<String>,
    patchable: BTreeMap<String, PatchablePageContext>,
}

#[derive(Debug, Clone)]
struct PatchablePageContext {
    path: String,
    title: String,
    kind: String,
    body: String,
    body_sha256: String,
    sections: Vec<MarkdownSection>,
}

#[derive(Debug, Clone)]
struct MarkdownSection {
    anchor: String,
    level: usize,
    start: usize,
    end: usize,
    sha256: String,
}

impl ExistingPageIndex {
    fn from_pages(pages: &[BriefingPage], patchable_pages: &[PatchablePageContext]) -> Self {
        Self {
            paths: pages.iter().map(|page| page.path.clone()).collect(),
            titles: pages
                .iter()
                .map(|page| normalize_title(&page.title))
                .filter(|title| !title.is_empty())
                .collect(),
            patchable: patchable_pages
                .iter()
                .map(|page| (page.path.clone(), page.clone()))
                .collect(),
        }
    }

    fn contains_path(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    fn contains_title(&self, title: &str) -> bool {
        let normalized = normalize_title(title);
        !normalized.is_empty() && self.titles.contains(&normalized)
    }

    fn patchable(&self, path: &str) -> Option<&PatchablePageContext> {
        self.patchable.get(path)
    }
}

fn normalize_title(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

async fn load_patchable_pages(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    recent_pages: &[BriefingPage],
    cfg: &AutoImproveReviewConfig,
) -> AutoImproveResult<Vec<PatchablePageContext>> {
    let mut out = Vec::new();
    for page in recent_pages
        .iter()
        .filter(|p| p.path.starts_with("_rules/") || p.path.starts_with("procedures/"))
        .take(cfg.max_patchable_pages)
    {
        if let Some(body) = reader
            .page_body_by_ids(workspace_id, project_id, &page.path)
            .await?
        {
            let sections = parse_markdown_sections(&body.body);
            out.push(PatchablePageContext {
                path: page.path.clone(),
                title: page.title.clone(),
                kind: page.kind.clone(),
                body_sha256: sha256_hex(body.body.as_bytes()),
                body: body.body,
                sections,
            });
        }
    }
    Ok(out)
}

async fn load_rejection_context(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    cfg: &AutoImproveReviewConfig,
) -> AutoImproveResult<Vec<AutoImproveRejectionSummary>> {
    if cfg.max_rejection_context == 0 || cfg.rejection_context_days == 0 {
        return Ok(Vec::new());
    }
    let age_micros = i64::from(cfg.rejection_context_days)
        .saturating_mul(24)
        .saturating_mul(60)
        .saturating_mul(60)
        .saturating_mul(1_000_000);
    let since = jiff::Timestamp::now()
        .as_microsecond()
        .saturating_sub(age_micros);
    Ok(reader
        .recent_auto_improve_rejections(
            workspace_id,
            project_id,
            cfg.max_rejection_context,
            Some(since),
        )
        .await?)
}

#[allow(clippy::too_many_arguments)]
fn build_prompt_input(
    session_id: SessionId,
    observations: &[Observation],
    duration_secs: u64,
    session_page: Option<&StoredPageBody>,
    recent_pages: &[BriefingPage],
    patchable_pages: &[PatchablePageContext],
    rejection_context: &[AutoImproveRejectionSummary],
    cfg: &AutoImproveReviewConfig,
) -> PromptInput {
    let mut warnings = Vec::new();
    let mut rejected_candidates = Vec::new();
    let usable_tokens = cfg.max_input_tokens.saturating_sub(PROMPT_RESERVE_TOKENS);
    let usable_chars = usable_tokens.saturating_mul(CHARS_PER_TOKEN);

    let recent = render_recent_pages(recent_pages);
    let patchable_budget = usable_chars / 4;
    let patchable = render_patchable_pages(
        patchable_pages,
        cfg.max_patchable_body_chars,
        patchable_budget,
        &mut warnings,
    );
    let session_page_budget = session_page
        .map(|_| usable_chars / 3)
        .unwrap_or(0)
        .min(MAX_SESSION_PAGE_CHARS);
    let session_page_section =
        render_session_page(session_page, session_page_budget, &mut warnings);
    let rejection_budget = rejection_context_char_budget(usable_chars);
    let rejection_text = render_rejection_context(rejection_context, rejection_budget);
    let observation_budget = usable_chars
        .saturating_sub(recent.len())
        .saturating_sub(patchable.text.len())
        .saturating_sub(rejection_text.len())
        .saturating_sub(session_page_section.len())
        .saturating_sub(PROMPT_SCAFFOLD_RESERVE_CHARS);
    let selected_limit = if session_page.is_some() {
        SAMPLE_LIMIT_WITH_SESSION_PAGE
    } else {
        SAMPLE_LIMIT_WITHOUT_SESSION_PAGE
    };
    let projected_observations = project_observations(
        observations,
        &ObservationProjectionConfig::new(
            observation_budget,
            selected_limit,
            MAX_OBSERVATION_BODY_CHARS,
        )
        .with_context_label("auto-improve"),
    );
    warnings.extend(projected_observations.warnings.iter().cloned());
    let rendered_observations = projected_observations.text;
    let selected_count = projected_observations.selected_count;
    let patchable_text = &patchable.text;

    if selected_count < observations.len() {
        rejected_candidates.push(AutoImproveRejectedCandidate {
            reason: "input_budget_sampled".into(),
            evidence: format!(
                "selected {selected_count} of {} observations for scalable review",
                observations.len()
            ),
            target_path: None,
            kind: None,
            operation: None,
            edit_mode: None,
        });
    }

    let prompt = format!(
        "Review one completed ai-memory session and propose only durable wiki edits.\n\
         Session: {session_id}\n\
         Observation count: {}\n\
         Session duration seconds: {duration_secs}\n\
         Minimum confidence: {}\n\
         Max proposals: {}\n\
         Include raw fallback: {}\n\
         Pending proposal sidecar path: {}\n\n\
         Existing project pages, for duplicate avoidance. Full-page proposals must not target these existing paths or titles except _slots/current-focus.md:\n{recent}\n\n\
         Patchable existing target context. Patch mode is allowed only for these listed _rules/ and procedures/ paths. Use exact markdown heading anchors including markers. Do not request patches for pages, anchors, or bodies not included here. delete_section is unsupported. Budgets: max edits/proposal {}, max patch edits/run {}, max content chars/edit {}, max changed chars/proposal {}, max final body chars {}, max rule page tokens {}, max procedure page tokens {}.\n{patchable_text}\n\n\
         Recent rejected auto-improvement attempts for this workspace/project. Avoid repeating these target/reason/fingerprint patterns unless the session provides clearly new evidence:\n{rejection_text}\n\n\
         Consolidated session page, primary source when present:\n{session_page_section}\n\n\
         Selected observations, sampled for scale and supporting evidence:\n{rendered_observations}\n\n\
         Return proposals only for durable lessons. Prefer gotchas/, decisions/, concepts/, procedures/, _rules/, _slots/current-focus.md, or notes/. Use _rules/ exactly for rules; never rules/. Full-page proposal body markdown must begin with '# <title>'; patch proposals use edits and are materialized by validation. Full-page and materialized patch final bodies for _rules/ and procedures/ must stay within their page token budgets. Reject no-activity sessions, smoke tests, release markers, one-off task narratives, transient failures, generic routing snippets, and agent instruction text unless the session explicitly changed project policy.",
        observations.len(),
        cfg.min_confidence,
        cfg.max_proposals_per_run,
        cfg.include_raw_fallback,
        cfg.pending_path,
        cfg.max_edits_per_proposal,
        cfg.max_patch_edits_per_run,
        cfg.max_edit_content_chars,
        cfg.max_changed_chars_per_proposal,
        cfg.max_final_body_chars,
        cfg.max_rule_page_tokens,
        cfg.max_procedure_page_tokens,
    );

    PromptInput {
        prompt,
        patchable_paths: patchable.included_paths,
        rejected_candidates,
        warnings,
    }
}

fn rejection_context_char_budget(usable_chars: usize) -> usize {
    (usable_chars / 8).min(MAX_REJECTION_CONTEXT_CHARS)
}

fn render_rejection_context(
    rejections: &[AutoImproveRejectionSummary],
    max_total_chars: usize,
) -> String {
    if rejections.is_empty() || max_total_chars == 0 {
        return "(none)".into();
    }
    let mut out = String::new();
    for rejection in rejections {
        let line = format!(
            "- path: {}; kind: {}; op: {}; edit_mode: {}; reason: {}; fingerprint: {}; summary: {}",
            truncate_field(
                rejection.target_path.as_deref().unwrap_or("(none)"),
                MAX_REJECTION_PATH_CHARS
            ),
            truncate_field(
                rejection.kind.as_deref().unwrap_or("(none)"),
                MAX_REJECTION_PATH_CHARS
            ),
            truncate_field(
                rejection.operation.as_deref().unwrap_or("(none)"),
                MAX_REJECTION_PATH_CHARS
            ),
            truncate_field(
                rejection.edit_mode.as_deref().unwrap_or("(none)"),
                MAX_REJECTION_PATH_CHARS
            ),
            truncate_field(&rejection.reason, MAX_REJECTION_REASON_CHARS),
            truncate_field(
                &rejection.normalized_fingerprint,
                MAX_REJECTION_FINGERPRINT_CHARS
            ),
            truncate_field(&rejection.summary, MAX_REJECTION_SUMMARY_CHARS)
        );
        let separator_len = usize::from(!out.is_empty());
        if out.len() + separator_len + line.len() <= max_total_chars {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&line);
            continue;
        }

        let remaining = max_total_chars.saturating_sub(out.len() + separator_len);
        if remaining > 0 {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&truncate_to_budget(
                &line,
                remaining,
                " [rejection context truncated]",
            ));
        }
        break;
    }
    if out.is_empty() { "(none)".into() } else { out }
}

fn truncate_field(input: &str, max_chars: usize) -> String {
    truncate_to_budget(input, max_chars, " [truncated]")
}

fn render_session_page(
    page: Option<&StoredPageBody>,
    max_chars: usize,
    warnings: &mut Vec<String>,
) -> String {
    let Some(page) = page else {
        return "(none; relying on sampled observations)".into();
    };
    if max_chars == 0 {
        warnings.push("session page exists but input budget left no room for it".into());
        return "(omitted by input budget)".into();
    }
    let (body, truncated) = truncate_with_marker(&page.body, max_chars, "[session page truncated]");
    if truncated {
        warnings.push(format!(
            "session page body truncated to {max_chars} chars before review"
        ));
    }
    format!(
        "title: {}\ntier: {}\npinned: {}\nbody:\n{}\n",
        page.title, page.tier, page.pinned, body
    )
}

fn render_recent_pages(pages: &[BriefingPage]) -> String {
    if pages.is_empty() {
        return "(none)".into();
    }
    let mut out = String::new();
    for page in pages {
        out.push_str(&format!(
            "- {} | {} | {} | updated {}\n",
            page.path, page.title, page.kind, page.updated_at
        ));
    }
    out
}

fn render_patchable_pages(
    pages: &[PatchablePageContext],
    max_body_chars: usize,
    max_total_chars: usize,
    warnings: &mut Vec<String>,
) -> RenderedPatchablePages {
    if pages.is_empty() || max_total_chars == 0 {
        return RenderedPatchablePages {
            text: "(none)".into(),
            included_paths: BTreeSet::new(),
        };
    }
    let mut out = String::new();
    let mut included_paths = BTreeSet::new();
    for page in pages {
        let mut header = format!(
            "path: {}\ntitle: {}\nkind: {}\nbody_sha256: {}\nanchors:\n",
            page.path, page.title, page.kind, page.body_sha256
        );
        for section in &page.sections {
            header.push_str(&format!(
                "- {} | section_sha256: {}\n",
                section.anchor, section.sha256
            ));
        }
        let (body, truncated) = truncate_with_marker(
            &page.body,
            max_body_chars,
            "[patchable target body truncated]",
        );
        if truncated {
            warnings.push(format!(
                "patchable target {} body truncated to {max_body_chars} chars before review",
                page.path
            ));
        }
        let full = format!("{header}body:\n```markdown\n{body}\n```\n\n");
        let outline = format!("{header}body: (omitted by patchable context budget)\n\n");
        if out.len() + full.len() <= max_total_chars {
            out.push_str(&full);
            included_paths.insert(page.path.clone());
        } else if out.len() + outline.len() <= max_total_chars {
            warnings.push(format!(
                "patchable target {} body omitted by patchable context budget",
                page.path
            ));
            out.push_str(&outline);
            included_paths.insert(page.path.clone());
        } else {
            warnings.push("patchable target context truncated by input budget".into());
            break;
        }
    }
    RenderedPatchablePages {
        text: if out.is_empty() { "(none)".into() } else { out },
        included_paths,
    }
}

fn parse_markdown_sections(body: &str) -> Vec<MarkdownSection> {
    let headings = heading_positions(body);
    let mut sections = Vec::new();
    for (idx, (start, level, anchor)) in headings.iter().enumerate() {
        let end = headings[idx + 1..]
            .iter()
            .find(|(_, next_level, _)| next_level <= level)
            .map(|(pos, _, _)| *pos)
            .unwrap_or(body.len());
        sections.push(MarkdownSection {
            anchor: anchor.clone(),
            level: *level,
            start: *start,
            end,
            sha256: sha256_hex(&body.as_bytes()[*start..end]),
        });
    }
    sections
}

fn heading_positions(body: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut offset = 0;
    let mut in_fence = false;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if is_fence_line(trimmed) {
            in_fence = !in_fence;
        } else if !in_fence && let Some(level) = heading_level(trimmed) {
            out.push((offset, level, trimmed.to_string()));
        }
        offset += line.len();
    }
    out
}

fn is_fence_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn heading_level(line: &str) -> Option<usize> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line.as_bytes().get(hashes) == Some(&b' ') {
        Some(hashes)
    } else {
        None
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn truncate_with_marker(input: &str, max_bytes: usize, marker: &str) -> (String, bool) {
    if input.len() <= max_bytes {
        return (input.to_string(), false);
    }
    let mut end = max_bytes.min(input.len());
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = input[..end].to_string();
    out.push('\n');
    out.push_str(marker);
    (out, true)
}

fn truncate_to_budget(input: &str, max_bytes: usize, marker: &str) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    if max_bytes == 0 {
        return String::new();
    }

    let marker_start = if marker.len() < max_bytes {
        max_bytes - marker.len()
    } else {
        0
    };
    let mut end = marker_start.min(input.len());
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }

    let mut out = input[..end].to_string();
    if out.len() + marker.len() <= max_bytes {
        out.push_str(marker);
    } else {
        let mut marker_end = max_bytes.saturating_sub(out.len()).min(marker.len());
        while marker_end > 0 && !marker.is_char_boundary(marker_end) {
            marker_end -= 1;
        }
        out.push_str(&marker[..marker_end]);
    }
    out
}

fn preflight_rejection(
    observations: &[Observation],
    duration_secs: u64,
    cfg: &AutoImproveReviewConfig,
) -> Option<AutoImproveRejectedCandidate> {
    if observations.len() < cfg.min_observations {
        return Some(AutoImproveRejectedCandidate {
            reason: "too_few_observations".into(),
            evidence: format!(
                "{} observations below configured minimum {}",
                observations.len(),
                cfg.min_observations
            ),
            target_path: None,
            kind: None,
            operation: None,
            edit_mode: None,
        });
    }
    if duration_secs < cfg.min_session_duration_secs {
        return Some(AutoImproveRejectedCandidate {
            reason: "session_too_short".into(),
            evidence: format!(
                "{duration_secs}s below configured minimum {}s",
                cfg.min_session_duration_secs
            ),
            target_path: None,
            kind: None,
            operation: None,
            edit_mode: None,
        });
    }
    None
}

fn validate_response(
    raw: AutoImproveLlmResponse,
    cfg: &AutoImproveReviewConfig,
    existing_index: &ExistingPageIndex,
) -> (
    Vec<AutoImproveProposal>,
    Vec<AutoImproveRejectedCandidate>,
    Vec<String>,
) {
    let mut proposals = Vec::new();
    let mut rejected = raw.rejected_candidates;
    let mut warnings = Vec::new();
    let mut patch_edits_used = 0usize;

    for mut proposal in raw.proposals {
        if proposals.len() >= cfg.max_proposals_per_run {
            rejected.push(AutoImproveRejectedCandidate {
                reason: "max_proposals_exceeded".into(),
                evidence: proposal.path.clone(),
                target_path: Some(proposal.path),
                kind: Some(proposal.kind),
                operation: Some(proposal.operation),
                edit_mode: Some(proposal.edit_mode),
            });
            continue;
        }
        normalize_proposal(&mut proposal, &mut warnings);
        match validate_proposal(&mut proposal, cfg, existing_index) {
            Ok(()) => {
                if proposal.edit_mode == "patch" {
                    let edit_count = proposal.edits.len();
                    if patch_edits_used.saturating_add(edit_count) > cfg.max_patch_edits_per_run {
                        rejected.push(AutoImproveRejectedCandidate {
                            reason: "patch_run_edit_budget_exceeded".into(),
                            evidence: proposal.path.clone(),
                            target_path: Some(proposal.path),
                            kind: Some(proposal.kind),
                            operation: Some(proposal.operation),
                            edit_mode: Some(proposal.edit_mode),
                        });
                        continue;
                    }
                    patch_edits_used += edit_count;
                }
                proposals.push(proposal);
            }
            Err(reason) => rejected.push(AutoImproveRejectedCandidate {
                reason,
                evidence: proposal.path.clone(),
                target_path: Some(proposal.path),
                kind: Some(proposal.kind),
                operation: Some(proposal.operation),
                edit_mode: Some(proposal.edit_mode),
            }),
        }
    }

    if raw.summary.trim().is_empty() {
        warnings.push("LLM returned an empty review summary".into());
    }

    (proposals, rejected, warnings)
}

fn normalize_proposal(proposal: &mut AutoImproveProposal, warnings: &mut Vec<String>) {
    normalize_kind(proposal, warnings);

    if proposal.edit_mode.trim().is_empty() {
        proposal.edit_mode = default_edit_mode();
    }
    if proposal.edit_mode == "patch" {
        return;
    }

    if proposal.body_markdown.trim_start().starts_with("# ") {
        return;
    }
    let title = proposal.title.trim();
    let body = proposal.body_markdown.trim_start();
    if title.is_empty() || body.is_empty() {
        return;
    }
    proposal.body_markdown = format!("# {title}\n\n{body}");
    warnings.push(format!(
        "proposal {} body lacked an H1; prepended title as H1 before validation",
        proposal.path
    ));
}

fn normalize_kind(proposal: &mut AutoImproveProposal, warnings: &mut Vec<String>) {
    let original = proposal.kind.clone();
    let alias = canonical_kind_alias(&original);
    if !alias.is_empty() && kind_matches_path(&alias, &proposal.path) {
        proposal.kind = alias;
    } else if original.trim().is_empty()
        && let Some(kind) = canonical_kind_for_path(&proposal.path)
    {
        proposal.kind = kind.into();
    }

    if proposal.kind != original {
        warnings.push(format!(
            "proposal {} kind normalized from {:?} to {:?}",
            proposal.path, original, proposal.kind
        ));
    }
}

fn canonical_kind_alias(kind: &str) -> String {
    match kind.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "gotcha" | "gotchas" | "pitfall" | "pitfalls" => "gotcha".into(),
        "decision" | "decisions" | "design_decision" | "architecture_decision" => "decision".into(),
        "concept" | "concepts" | "architecture" | "domain_knowledge" => "concept".into(),
        "procedure" | "procedures" | "process" | "workflow" | "workflows" => "procedure".into(),
        "rule" | "rules" | "policy" | "policies" => "rule".into(),
        "slot" | "slots" => "slot".into(),
        "fact" | "facts" => "fact".into(),
        "note" | "notes" => "note".into(),
        _ => String::new(),
    }
}

fn canonical_kind_for_path(path: &str) -> Option<&'static str> {
    if path.starts_with("gotchas/") {
        Some("gotcha")
    } else if path.starts_with("decisions/") {
        Some("decision")
    } else if path.starts_with("concepts/") {
        Some("concept")
    } else if path.starts_with("procedures/") {
        Some("procedure")
    } else if path.starts_with("_rules/") {
        Some("rule")
    } else if path.starts_with("_slots/") {
        Some("slot")
    } else if path.starts_with("notes/") {
        Some("note")
    } else {
        None
    }
}

fn validate_proposal(
    proposal: &mut AutoImproveProposal,
    cfg: &AutoImproveReviewConfig,
    existing_index: &ExistingPageIndex,
) -> Result<(), String> {
    if proposal.operation != "create_or_update" {
        return Err("unsupported_operation".into());
    }
    if proposal.confidence < cfg.min_confidence {
        return Err("confidence_below_threshold".into());
    }
    if proposal.rationale.trim().is_empty() {
        return Err("missing_rationale".into());
    }
    if proposal.evidence.is_empty()
        || proposal
            .evidence
            .iter()
            .any(|e| e.page.trim().is_empty() || e.quote.trim().is_empty())
    {
        return Err("missing_evidence".into());
    }
    let path = PagePath::new(proposal.path.clone()).map_err(|_| "invalid_path".to_string())?;
    match proposal.edit_mode.as_str() {
        "" | "full_page" => {
            validate_full_page_proposal(proposal, cfg, existing_index, path.as_str())
        }
        "patch" => validate_patch_proposal(proposal, cfg, existing_index, path.as_str()),
        _ => Err("unsupported_edit_mode".into()),
    }
}

fn validate_full_page_proposal(
    proposal: &AutoImproveProposal,
    cfg: &AutoImproveReviewConfig,
    existing_index: &ExistingPageIndex,
    path: &str,
) -> Result<(), String> {
    if proposal.body_markdown.trim().is_empty() {
        return Err("empty_body".into());
    }
    if proposal.body_markdown.len() > cfg.max_final_body_chars.min(MAX_PROPOSAL_BODY_CHARS) {
        return Err("body_too_large".into());
    }
    enforce_page_budget(path, &proposal.body_markdown, cfg)?;
    if !proposal.body_markdown.trim_start().starts_with("# ") {
        return Err("body_missing_h1".into());
    }
    if !allowed_target_path(path) {
        return Err("unsupported_path_prefix".into());
    }
    if path != "_slots/current-focus.md" && existing_index.contains_path(path) {
        return Err("duplicate_existing_path".into());
    }
    if path != "_slots/current-focus.md" && existing_index.contains_title(&proposal.title) {
        return Err("duplicate_existing_title".into());
    }
    if !kind_matches_path(&proposal.kind, path) {
        return Err("kind_path_mismatch".into());
    }
    Ok(())
}

fn validate_patch_proposal(
    proposal: &mut AutoImproveProposal,
    cfg: &AutoImproveReviewConfig,
    existing_index: &ExistingPageIndex,
    path: &str,
) -> Result<(), String> {
    if !(path.starts_with("_rules/") || path.starts_with("procedures/")) {
        return Err("patch_unsupported_path".into());
    }
    let Some(target) = existing_index.patchable(path) else {
        return Err("patch_target_not_in_context".into());
    };
    if proposal.edits.is_empty() {
        return Err("patch_missing_edits".into());
    }
    if proposal.edits.len() > cfg.max_edits_per_proposal {
        return Err("patch_too_many_edits".into());
    }
    let changed_chars: usize = proposal.edits.iter().map(|e| e.content.len()).sum();
    if changed_chars > cfg.max_changed_chars_per_proposal {
        return Err("patch_changed_chars_too_large".into());
    }
    if proposal
        .edits
        .iter()
        .any(|e| e.content.len() > cfg.max_edit_content_chars)
    {
        return Err("patch_edit_content_too_large".into());
    }
    if has_duplicate_headings(&target.body) {
        return Err("duplicate_anchor".into());
    }
    let body = materialize_patch(&target.body, &proposal.edits)?;
    if body.len() > cfg.max_final_body_chars.min(MAX_PROPOSAL_BODY_CHARS) {
        return Err("body_too_large".into());
    }
    enforce_page_budget(path, &body, cfg)?;
    if !body.trim_start().starts_with("# ") {
        return Err("body_missing_h1".into());
    }
    if h1_count(&body) != 1 {
        return Err("patch_extra_h1".into());
    }
    if has_duplicate_headings(&body) {
        return Err("duplicate_anchor".into());
    }
    if !kind_matches_path(&proposal.kind, path) {
        return Err("kind_path_mismatch".into());
    }
    proposal.body_markdown = body;
    proposal.expected_base_body_sha256 = Some(target.body_sha256.clone());
    Ok(())
}

fn enforce_page_budget(
    path: &str,
    body: &str,
    cfg: &AutoImproveReviewConfig,
) -> Result<(), String> {
    if path.starts_with("_rules/") {
        let max_chars = cfg.max_rule_page_tokens.saturating_mul(CHARS_PER_TOKEN);
        if body.len() > max_chars {
            return Err("rule_page_budget_exceeded".into());
        }
    } else if path.starts_with("procedures/") {
        let max_chars = cfg
            .max_procedure_page_tokens
            .saturating_mul(CHARS_PER_TOKEN);
        if body.len() > max_chars {
            return Err("procedure_page_budget_exceeded".into());
        }
    }
    Ok(())
}

fn materialize_patch(body: &str, edits: &[AutoImprovePatchEdit]) -> Result<String, String> {
    let mut current = body.to_string();
    for edit in edits {
        if edit.op == "delete_section" {
            return Err("unsupported_patch_op".into());
        }
        let sections = parse_markdown_sections(&current);
        if duplicate_heading_count(&sections, &edit.anchor) > 1 {
            return Err("duplicate_anchor".into());
        }
        let section = sections
            .iter()
            .find(|section| section.anchor == edit.anchor)
            .ok_or_else(|| "patch_anchor_not_found".to_string())?;
        let section_level = section.level;
        let section_start = section.start;
        let section_end = section.end;
        if section.level == 1 {
            return Err("patch_h1_anchor_unsupported".into());
        }
        match edit.op.as_str() {
            "append" => {
                let insert =
                    bounded_markdown_block(&edit.content, true, section_end < current.len());
                current.insert_str(section_end, &insert);
            }
            "add_section" => {
                let inserted_level = first_heading_level(&edit.content)
                    .ok_or_else(|| "add_section_missing_heading".to_string())?;
                if inserted_level == 1 {
                    return Err("add_section_h1_unsupported".into());
                }
                if inserted_level != section_level {
                    return Err("add_section_heading_level_mismatch".into());
                }
                let insert =
                    bounded_markdown_block(&edit.content, true, section_end < current.len());
                current.insert_str(section_end, &insert);
            }
            "replace_section" => {
                let expected = edit
                    .section_sha256
                    .as_deref()
                    .ok_or_else(|| "replace_section_missing_hash".to_string())?;
                if expected != section.sha256 {
                    return Err("replace_section_hash_mismatch".into());
                }
                let replacement_anchor = first_nonblank_line(&edit.content)
                    .ok_or_else(|| "replace_section_missing_heading".to_string())?;
                if replacement_anchor != edit.anchor {
                    return Err("replace_section_anchor_mismatch".into());
                }
                let replacement =
                    bounded_markdown_block(&edit.content, false, section_end < current.len());
                current.replace_range(
                    section_start..section_end,
                    replacement.trim_start_matches('\n'),
                );
            }
            _ => return Err("unsupported_patch_op".into()),
        }
    }
    Ok(current)
}

fn bounded_markdown_block(content: &str, prefix_blank: bool, suffix_blank: bool) -> String {
    let mut out = String::new();
    if prefix_blank {
        out.push_str("\n\n");
    }
    out.push_str(content.trim());
    if suffix_blank {
        out.push_str("\n\n");
    } else {
        out.push('\n');
    }
    out
}

fn first_heading_level(content: &str) -> Option<usize> {
    first_nonblank_line(content).and_then(heading_level)
}

fn first_nonblank_line(content: &str) -> Option<&str> {
    content
        .lines()
        .map(|line| line.trim_end_matches('\r'))
        .find(|line| !line.trim().is_empty())
}

fn has_duplicate_headings(body: &str) -> bool {
    let mut seen = BTreeSet::new();
    parse_markdown_sections(body)
        .into_iter()
        .map(|section| normalize_title(&section.anchor))
        .any(|anchor| !seen.insert(anchor))
}

fn duplicate_heading_count(sections: &[MarkdownSection], anchor: &str) -> usize {
    let normalized = normalize_title(anchor);
    sections
        .iter()
        .filter(|section| normalize_title(&section.anchor) == normalized)
        .count()
}

fn h1_count(body: &str) -> usize {
    parse_markdown_sections(body)
        .into_iter()
        .filter(|section| section.level == 1)
        .count()
}

fn allowed_target_path(path: &str) -> bool {
    path.starts_with("gotchas/")
        || path.starts_with("decisions/")
        || path.starts_with("concepts/")
        || path.starts_with("procedures/")
        || path.starts_with("_rules/")
        || path.starts_with("notes/")
        || path == "_slots/current-focus.md"
}

fn kind_matches_path(kind: &str, path: &str) -> bool {
    match kind {
        "gotcha" => path.starts_with("gotchas/"),
        "decision" => path.starts_with("decisions/"),
        "concept" => path.starts_with("concepts/"),
        "procedure" => path.starts_with("procedures/"),
        "rule" => path.starts_with("_rules/"),
        "slot" => path.starts_with("_slots/"),
        "fact" | "note" => path.starts_with("notes/") || path.starts_with("concepts/"),
        _ => false,
    }
}

fn session_duration_secs(observations: &[Observation]) -> u64 {
    let (Some(first), Some(last)) = (observations.first(), observations.last()) else {
        return 0;
    };
    let diff_us = last
        .created_at
        .as_microsecond()
        .saturating_sub(first.created_at.as_microsecond());
    u64::try_from(diff_us / 1_000_000).unwrap_or(0)
}

fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(CHARS_PER_TOKEN)
}

const AUTO_IMPROVE_SYSTEM_PROMPT: &str = r#"You are ai-memory's review-gated auto-improvement reviewer.

Return structured JSON matching the schema. You are proposing wiki edits, not applying them.

Only propose durable, future-useful knowledge:
- gotchas: reproducible pitfalls with a root cause and mitigation
- decisions: choices with rationale and consequences
- concepts: stable architecture or domain knowledge
- procedures: reusable multi-step workflows
- rules: explicit always/never instructions
- notes: useful facts that do not fit stronger categories

Reject transient setup failures, smoke tests, release markers, one-off task narratives, broad negative tool claims, and failures that were resolved without a reusable lesson.

Every proposal must include bounded evidence quotes, confidence, rationale, and a valid path. Full-page proposals must include markdown beginning with an H1; patch proposals must include anchored edits and are materialized before staging. Rule paths must start with `_rules/`, not `rules/`. Do not target sessions/ or _pending/. Treat the consolidated session page as primary when present; selected observations are supporting context. Do not promote ai-memory routing snippets, AGENTS.md/CLAUDE.md instructions, or generic agent tool guidance unless the session explicitly changed project policy."#;

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{
        AgentKind, NewObservation, NewSession, ObservationId, ObservationKind, Sanitized, Sanitizer,
    };
    use ai_memory_llm::{ChatResponse, LlmResult};
    use ai_memory_store::Store;
    use jiff::Timestamp;
    use tempfile::TempDir;

    struct FakeLlm;

    #[async_trait::async_trait]
    impl LlmProvider for FakeLlm {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn model(&self) -> &str {
            "fake-model"
        }

        async fn complete(&self, _request: ChatRequest) -> LlmResult<ChatResponse> {
            Ok(ChatResponse {
                text: "unused".into(),
                usage: None,
                model: "fake-model".into(),
            })
        }

        async fn complete_structured_raw(
            &self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> LlmResult<serde_json::Value> {
            Ok(serde_json::json!({
                "summary": "found one durable procedure",
                "proposals": [{
                    "operation": "create_or_update",
                    "path": "procedures/release.md",
                    "title": "Release Procedure",
                    "kind": "procedure",
                    "confidence": 0.91,
                    "rationale": "The session repeated a release workflow with verification.",
                    "evidence": [{"page": "sessions/test.md", "quote": "run the full gate before release"}],
                    "body_markdown": "# Release Procedure\n\nRun the full gate before release."
                }],
                "rejected_candidates": []
            }))
        }
    }

    fn cfg() -> AutoImproveReviewConfig {
        AutoImproveReviewConfig {
            min_observations: 3,
            min_session_duration_secs: 60,
            min_confidence: 0.75,
            max_input_tokens: 24_000,
            max_proposals_per_run: 2,
            include_raw_fallback: false,
            proposal_actor: "auto_improve".into(),
            pending_path: "_pending/auto-improve".into(),
            max_patchable_pages: DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_PAGES,
            max_patchable_body_chars: DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_BODY_CHARS,
            max_edits_per_proposal: DEFAULT_AUTO_IMPROVE_MAX_EDITS_PER_PROPOSAL,
            max_edit_content_chars: DEFAULT_AUTO_IMPROVE_MAX_EDIT_CONTENT_CHARS,
            max_changed_chars_per_proposal: DEFAULT_AUTO_IMPROVE_MAX_CHANGED_CHARS_PER_PROPOSAL,
            max_patch_edits_per_run: DEFAULT_AUTO_IMPROVE_MAX_PATCH_EDITS_PER_RUN,
            max_rejection_context: DEFAULT_AUTO_IMPROVE_MAX_REJECTION_CONTEXT,
            rejection_context_days: DEFAULT_AUTO_IMPROVE_REJECTION_CONTEXT_DAYS,
            max_final_body_chars: DEFAULT_AUTO_IMPROVE_MAX_FINAL_BODY_CHARS,
            max_rule_page_tokens: DEFAULT_AUTO_IMPROVE_MAX_RULE_PAGE_TOKENS,
            max_procedure_page_tokens: DEFAULT_AUTO_IMPROVE_MAX_PROCEDURE_PAGE_TOKENS,
            eval: AutoImproveEvalConfig::default(),
        }
    }

    fn proposal(path: &str, kind: &str, confidence: f32) -> AutoImproveProposal {
        AutoImproveProposal {
            operation: "create_or_update".into(),
            path: path.into(),
            title: "Test".into(),
            kind: kind.into(),
            confidence,
            rationale: "durable lesson".into(),
            evidence: vec![AutoImproveEvidence {
                page: "sessions/abc.md".into(),
                quote: "quote".into(),
            }],
            body_markdown: "# Test\n\nBody".into(),
            edit_mode: default_edit_mode(),
            edits: Vec::new(),
            expected_base_body_sha256: None,
        }
    }

    fn eval_cfg(command: String) -> AutoImproveEvalConfig {
        AutoImproveEvalConfig {
            enabled: true,
            command,
            timeout_secs: 2,
            targets: default_auto_improve_eval_targets(),
            min_delta: 0.01,
        }
    }

    #[cfg(unix)]
    fn write_eval_script(body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap().keep();
        let path = dir.join("eval.sh");
        std::fs::write(&path, body).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        // Execute through the shell instead of exec'ing the freshly-written
        // temp file directly. Some Linux filesystems can briefly report
        // ETXTBSY for direct exec under parallel tests even after write(2)
        // returns and the file handle has been dropped.
        format!("sh {}", path.display())
    }

    #[cfg(windows)]
    fn write_eval_script(body: &str) -> String {
        let dir = tempfile::TempDir::new().unwrap().keep();
        let path = dir.join("eval.cmd");
        let body = match body {
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"score_before\":0.72,\"score_after\":0.76,\"passed\":true}'\n" => {
                "more >NUL\r\necho {\"score_before\":0.72,\"score_after\":0.76,\"passed\":true}\r\n"
                    .into()
            }
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"score_before\":0.72,\"score_after\":0.70,\"passed\":true}'\n" => {
                "more >NUL\r\necho {\"score_before\":0.72,\"score_after\":0.70,\"passed\":true}\r\n"
                    .into()
            }
            "#!/bin/sh\ncat >/dev/null\nexit 7\n" => "more >NUL\r\nexit /B 7\r\n".into(),
            "#!/bin/sh\nexit 7\n" => "exit /B 7\r\n".into(),
            "#!/bin/sh\ncat >/dev/null\nprintf 'not-json'\n" => {
                "more >NUL\r\necho not-json\r\n".into()
            }
            "#!/bin/sh\ncat >/dev/null\nsleep 3\n" => {
                "more >NUL\r\nping -n 4 127.0.0.1 >NUL\r\n".into()
            }
            "#!/bin/sh\nsleep 5\n" => "ping -n 6 127.0.0.1 >NUL\r\n".into(),
            "#!/bin/sh\ni=0\nwhile [ $i -lt 70000 ]; do printf x; i=$((i + 1)); done\n" => {
                let chunk = "x".repeat(100);
                format!(
                    "set \"chunk={chunk}\"\r\nfor /L %%i in (1,1,700) do <NUL set /p _=%chunk%\r\n"
                )
            }
            other => panic!("unmapped eval script fixture for Windows: {other:?}"),
        };
        std::fs::write(&path, format!("@echo off\r\n{body}")).unwrap();
        format!("cmd.exe /C {}", path.display())
    }

    #[tokio::test]
    async fn eval_disabled_preserves_current_behavior() {
        let mut proposals = vec![proposal("_rules/test.md", "rule", 0.9)];
        let mut rejected = Vec::new();
        let mut warnings = Vec::new();
        apply_eval_gate_with_before_bodies(
            &AutoImproveEvalConfig::default(),
            &mut proposals,
            &mut rejected,
            &mut warnings,
            &BTreeMap::new(),
        )
        .await;
        assert_eq!(proposals.len(), 1);
        assert!(rejected.is_empty());
        assert!(warnings.is_empty());
    }

    #[tokio::test]
    async fn passing_eval_allows_proposal_and_records_evidence() {
        let script = write_eval_script(
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"score_before\":0.72,\"score_after\":0.76,\"passed\":true}'\n",
        );
        let mut proposals = vec![proposal("_rules/test.md", "rule", 0.9)];
        let mut rejected = Vec::new();
        let mut warnings = Vec::new();
        apply_eval_gate_with_before_bodies(
            &eval_cfg(script),
            &mut proposals,
            &mut rejected,
            &mut warnings,
            &BTreeMap::new(),
        )
        .await;
        assert_eq!(proposals.len(), 1);
        assert!(
            proposals[0]
                .evidence
                .iter()
                .any(|e| e.page == "auto_improve_eval")
        );
        assert!(rejected.is_empty());
    }

    #[tokio::test]
    async fn failing_eval_filters_and_records_rejection() {
        let script = write_eval_script(
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"score_before\":0.72,\"score_after\":0.70,\"passed\":true}'\n",
        );
        let mut proposals = vec![proposal("procedures/test.md", "procedure", 0.9)];
        let mut rejected = Vec::new();
        let mut warnings = Vec::new();
        apply_eval_gate_with_before_bodies(
            &eval_cfg(script),
            &mut proposals,
            &mut rejected,
            &mut warnings,
            &BTreeMap::new(),
        )
        .await;
        assert!(proposals.is_empty());
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].reason, "eval_gate_failed");
        assert_eq!(
            rejected[0].target_path.as_deref(),
            Some("procedures/test.md")
        );
    }

    #[tokio::test]
    async fn eval_error_timeout_and_invalid_json_fail_closed() {
        let cases = vec![
            (
                write_eval_script("#!/bin/sh\ncat >/dev/null\nexit 7\n"),
                "eval_gate_error",
            ),
            (
                write_eval_script("#!/bin/sh\ncat >/dev/null\nprintf 'not-json'\n"),
                "eval_gate_error",
            ),
            (
                write_eval_script("#!/bin/sh\ncat >/dev/null\nsleep 3\n"),
                "eval_gate_timeout",
            ),
        ];
        for (command, expected_reason) in cases {
            let mut cfg = eval_cfg(command);
            cfg.timeout_secs = 1;
            let mut proposals = vec![proposal("_rules/test.md", "rule", 0.9)];
            let mut rejected = Vec::new();
            let mut warnings = Vec::new();
            apply_eval_gate_with_before_bodies(
                &cfg,
                &mut proposals,
                &mut rejected,
                &mut warnings,
                &BTreeMap::new(),
            )
            .await;
            assert!(proposals.is_empty());
            assert_eq!(rejected[0].reason, expected_reason);
        }
    }

    #[tokio::test]
    async fn eval_timeout_covers_blocked_stdin_write() {
        let script = write_eval_script("#!/bin/sh\nsleep 5\n");
        let mut cfg = eval_cfg(script);
        cfg.timeout_secs = 1;
        let mut proposal = proposal("_rules/test.md", "rule", 0.9);
        proposal.body_markdown = "x".repeat(10 * 1024 * 1024);
        let mut proposals = vec![proposal];
        let mut rejected = Vec::new();
        let mut warnings = Vec::new();

        let started = std::time::Instant::now();
        apply_eval_gate_with_before_bodies(
            &cfg,
            &mut proposals,
            &mut rejected,
            &mut warnings,
            &BTreeMap::new(),
        )
        .await;

        assert!(proposals.is_empty());
        assert_eq!(rejected[0].reason, "eval_gate_timeout");
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn oversized_eval_stdout_fails_closed() {
        let script = write_eval_script(
            "#!/bin/sh\ni=0\nwhile [ $i -lt 70000 ]; do printf x; i=$((i + 1)); done\n",
        );
        let mut proposals = vec![proposal("_rules/test.md", "rule", 0.9)];
        let mut rejected = Vec::new();
        let mut warnings = Vec::new();
        apply_eval_gate_with_before_bodies(
            &eval_cfg(script),
            &mut proposals,
            &mut rejected,
            &mut warnings,
            &BTreeMap::new(),
        )
        .await;

        assert!(proposals.is_empty());
        // This test has flaked rarely on loaded machines with a different
        // eval_gate_error; print the actual reason/evidence so the next
        // occurrence identifies the real failure mode instead of hiding it.
        assert_eq!(
            rejected[0].reason, "eval_gate_error",
            "evidence: {}",
            rejected[0].evidence
        );
        assert!(
            rejected[0].evidence.contains("eval stdout exceeded"),
            "evidence: {}",
            rejected[0].evidence
        );
    }

    #[test]
    fn oversized_eval_reason_is_capped_before_rejection_evidence() {
        let outcome = AutoImproveEvalResponse {
            score_before: None,
            score_after: None,
            passed: false,
            reason: Some("a".repeat(MAX_EVAL_REASON_CHARS + 100)),
        };
        let evidence = format_eval_failure(&outcome, None, 0.0);

        assert!(evidence.contains("eval reason truncated"));
        assert!(evidence.len() < MAX_EVAL_REASON_CHARS + 200);
    }

    #[tokio::test]
    async fn non_targeted_proposal_bypasses_eval() {
        let script = write_eval_script("#!/bin/sh\nexit 7\n");
        let mut proposals = vec![proposal("gotchas/test.md", "gotcha", 0.9)];
        let mut rejected = Vec::new();
        let mut warnings = Vec::new();
        apply_eval_gate_with_before_bodies(
            &eval_cfg(script),
            &mut proposals,
            &mut rejected,
            &mut warnings,
            &BTreeMap::new(),
        )
        .await;
        assert_eq!(proposals.len(), 1);
        assert!(rejected.is_empty());
    }

    fn obs(
        idx: usize,
        kind: ObservationKind,
        title: &str,
        body: &str,
        importance: u8,
    ) -> Observation {
        Observation {
            id: ObservationId::new(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            session_id: SessionId::new(),
            kind,
            extension: None,
            source_event: None,
            title: title.into(),
            body: body.into(),
            importance,
            created_at: Timestamp::from_microsecond(idx as i64 * 1_000_000)
                .expect("test timestamp is valid"),
        }
    }

    #[tokio::test]
    async fn reviewer_returns_validated_llm_proposals_without_writes() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "proj", None)
            .await
            .unwrap();
        let session_id = ai_memory_core::SessionId::new();
        store
            .writer
            .begin_session(NewSession {
                id: session_id,
                workspace_id: ws,
                project_id: proj,
                agent_kind: AgentKind::Other,
                cwd: None,
            })
            .await
            .unwrap();
        for i in 0..3 {
            store
                .writer
                .insert_observation(Sanitized::new(
                    NewObservation {
                        session_id,
                        workspace_id: ws,
                        project_id: proj,
                        kind: if i == 0 {
                            ObservationKind::SessionStart
                        } else {
                            ObservationKind::UserPrompt
                        },
                        extension: None,
                        source_event: None,
                        title: format!("event {i}"),
                        body: "run the full gate before release".into(),
                        importance: 5,
                    },
                    &Sanitizer::builtin(),
                ))
                .await
                .unwrap();
        }

        let report = run_auto_improve_review(
            &store.reader,
            &FakeLlm,
            ws,
            proj,
            session_id,
            AutoImproveReviewConfig {
                min_session_duration_secs: 0,
                ..cfg()
            },
        )
        .await
        .unwrap();

        assert_eq!(report.provider, "fake");
        assert_eq!(report.model, "fake-model");
        assert_eq!(report.proposals.len(), 1);
        assert_eq!(report.proposals[0].path, "procedures/release.md");
        assert!(report.rejected_candidates.is_empty());
    }

    #[test]
    fn validation_accepts_procedure_pages() {
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![proposal("procedures/release.md", "procedure", 0.91)],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, warnings) =
            validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert_eq!(accepted.len(), 1);
        assert!(rejected.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn evidence_deserializes_bare_string_quotes() {
        let raw: AutoImproveLlmResponse = serde_json::from_value(serde_json::json!({
            "summary": "ok",
            "proposals": [{
                "operation": "create_or_update",
                "path": "procedures/release.md",
                "title": "Release Procedure",
                "kind": "procedure",
                "confidence": 0.91,
                "rationale": "The session repeated a release workflow with verification.",
                "evidence": ["run the full gate before release"],
                "body_markdown": "# Release Procedure\n\nRun the full gate before release."
            }],
            "rejected_candidates": []
        }))
        .unwrap();

        assert_eq!(raw.proposals[0].evidence.len(), 1);
        assert_eq!(raw.proposals[0].evidence[0].page, "unspecified");
        assert_eq!(
            raw.proposals[0].evidence[0].quote,
            "run the full gate before release"
        );

        let (accepted, rejected, warnings) =
            validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert_eq!(accepted.len(), 1);
        assert!(rejected.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn missing_operation_defaults_to_create_or_update() {
        let raw: AutoImproveLlmResponse = serde_json::from_value(serde_json::json!({
            "summary": "ok",
            "proposals": [{
                "path": "procedures/release.md",
                "title": "Release Procedure",
                "kind": "procedure",
                "confidence": 0.91,
                "rationale": "The session repeated a release workflow with verification.",
                "evidence": [{"quote": "run the full gate before release"}],
                "body_markdown": "# Release Procedure\n\nRun the full gate before release."
            }],
            "rejected_candidates": []
        }))
        .unwrap();

        assert_eq!(raw.proposals[0].operation, "create_or_update");
        assert_eq!(raw.proposals[0].evidence[0].page, "unspecified");
        assert_eq!(
            raw.proposals[0].evidence[0].quote,
            "run the full gate before release"
        );

        let (accepted, rejected, warnings) =
            validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert_eq!(accepted.len(), 1);
        assert!(rejected.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn malformed_proposal_missing_path_is_rejected_not_parse_fatal() {
        let raw: AutoImproveLlmResponse = serde_json::from_value(serde_json::json!({
            "summary": "ok",
            "proposals": [{
                "title": "Release Procedure",
                "kind": "procedure",
                "confidence": 0.91,
                "rationale": "The session repeated a release workflow with verification.",
                "evidence": ["run the full gate before release"],
                "body_markdown": "# Release Procedure\n\nRun the full gate before release."
            }],
            "rejected_candidates": []
        }))
        .unwrap();

        let (accepted, rejected, warnings) =
            validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert!(accepted.is_empty());
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].reason, "invalid_path");
        assert!(warnings.is_empty());
    }

    #[test]
    fn proposal_body_accepts_common_markdown_aliases() {
        let raw: AutoImproveLlmResponse = serde_json::from_value(serde_json::json!({
            "summary": "ok",
            "proposals": [{
                "path": "procedures/release.md",
                "title": "Release Procedure",
                "kind": "procedure",
                "confidence": 0.91,
                "rationale": "The session repeated a release workflow with verification.",
                "evidence": ["run the full gate before release"],
                "body": "# Release Procedure\n\nRun the full gate before release."
            }],
            "rejected_candidates": []
        }))
        .unwrap();

        assert!(
            raw.proposals[0]
                .body_markdown
                .starts_with("# Release Procedure")
        );

        let (accepted, rejected, warnings) =
            validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert_eq!(accepted.len(), 1);
        assert!(rejected.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn validation_normalizes_plural_kind_aliases() {
        let mut candidate = proposal("gotchas/workspace-scope.md", "gotchas", 0.91);
        candidate.title = "Workspace Scope Gotcha".into();
        candidate.body_markdown = "# Workspace Scope Gotcha\n\nBody".into();
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![candidate],
            rejected_candidates: Vec::new(),
        };

        let (accepted, rejected, warnings) =
            validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].kind, "gotcha");
        assert!(rejected.is_empty());
        assert!(warnings.iter().any(|w| w.contains("kind normalized")));
    }

    #[test]
    fn validation_derives_missing_kind_from_path() {
        let mut candidate = proposal("procedures/release.md", "", 0.91);
        candidate.title = "Release Procedure".into();
        candidate.body_markdown = "# Release Procedure\n\nBody".into();
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![candidate],
            rejected_candidates: Vec::new(),
        };

        let (accepted, rejected, warnings) =
            validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].kind, "procedure");
        assert!(rejected.is_empty());
        assert!(warnings.iter().any(|w| w.contains("kind normalized")));
    }

    #[test]
    fn prompt_uses_session_page_and_samples_high_signal_long_sessions() {
        let session_id = SessionId::new();
        let mut observations: Vec<Observation> = (0..200)
            .map(|idx| {
                obs(
                    idx,
                    ObservationKind::PostToolUse,
                    "routine",
                    "boring output",
                    3,
                )
            })
            .collect();
        observations[180] = obs(
            180,
            ObservationKind::PostToolUse,
            "root cause found",
            "root cause was a stale scope cache; fixed by re-resolving project scope after write errors",
            10,
        );
        let page = StoredPageBody {
            title: "Large Session Summary".into(),
            body: "# Large Session Summary\n\nThe session audited scalable project isolation."
                .into(),
            frontmatter_json: "{}".into(),
            tier: "episodic".into(),
            pinned: false,
        };

        let prompt = build_prompt_input(
            session_id,
            &observations,
            3_600,
            Some(&page),
            &[],
            &[],
            &[],
            &AutoImproveReviewConfig {
                max_input_tokens: 8_000,
                ..cfg()
            },
        );

        assert!(prompt.prompt.contains("Large Session Summary"));
        assert!(prompt.prompt.contains("root cause was a stale scope cache"));
        assert!(prompt.warnings.iter().any(|w| w.contains("sampled")));
        assert!(
            prompt
                .rejected_candidates
                .iter()
                .any(|r| r.reason == "input_budget_sampled")
        );
    }

    #[test]
    fn prompt_keeps_paired_safe_tool_observations_below_sampling_threshold() {
        let call_id = "stable-call-190";
        let observations = vec![
            obs(
                0,
                ObservationKind::PreToolUse,
                "tool non-file",
                &format!("tool_family: non-file\ntool_call_id: {call_id}"),
                5,
            ),
            obs(
                1,
                ObservationKind::PostToolUse,
                "tool non-file",
                &format!(
                    "tool_family: non-file\ntool_call_id: {call_id}\noutcome: unknown\n---\nPOST_OUTPUT_SURVIVES"
                ),
                5,
            ),
        ];
        assert!(observations.len() < 48);
        let prompt = build_prompt_input(
            SessionId::new(),
            &observations,
            3_600,
            None,
            &[],
            &[],
            &[],
            &cfg(),
        );
        assert_eq!(prompt.prompt.matches(call_id).count(), 2);
        assert!(prompt.prompt.contains("POST_OUTPUT_SURVIVES"));
        for sentinel in ["SENTINEL_COMMAND", "SENTINEL_PATH"] {
            assert!(!prompt.prompt.contains(sentinel));
        }
    }

    #[test]
    fn prompt_bounds_patchable_context_and_charges_observation_budget() {
        let session_id = SessionId::new();
        let observations: Vec<_> = (0..20)
            .map(|i| {
                obs(
                    i,
                    ObservationKind::PostToolUse,
                    &format!("obs {i}"),
                    "important reusable detail ".repeat(80).as_str(),
                    8,
                )
            })
            .collect();
        let body = format!("# Rule\n\n## Anchor\n{}", "body ".repeat(2_000));
        let patchable = vec![PatchablePageContext {
            path: "_rules/test.md".into(),
            title: "Test rule".into(),
            kind: "rule".into(),
            body_sha256: sha256_hex(body.as_bytes()),
            sections: parse_markdown_sections(&body),
            body,
        }];
        let prompt = build_prompt_input(
            session_id,
            &observations,
            3_600,
            None,
            &[],
            &patchable,
            &[],
            &AutoImproveReviewConfig {
                max_input_tokens: 2_500,
                max_patchable_body_chars: 512,
                ..cfg()
            },
        );

        assert!(prompt.prompt.contains("[patchable target body truncated]"));
        assert!(
            prompt
                .warnings
                .iter()
                .any(|w| w.contains("patchable target _rules/test.md body truncated"))
        );
        assert!(
            prompt
                .rejected_candidates
                .iter()
                .any(|r| r.reason == "input_budget_sampled")
        );
    }

    #[test]
    fn prompt_includes_rejection_context() {
        let session_id = SessionId::new();
        let observations = vec![obs(
            1,
            ObservationKind::PostToolUse,
            "durable",
            "avoid repeating failed auto-improve edits",
            10,
        )];
        let rejections = vec![AutoImproveRejectionSummary {
            id: "r1".into(),
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            target_path: Some("notes/repeat.md".into()),
            kind: Some("note".into()),
            operation: Some("create".into()),
            edit_mode: Some("full_page".into()),
            reason: "duplicate_existing_path".into(),
            normalized_fingerprint: "abc123".into(),
            summary: "Repeated note".into(),
            evidence_json: serde_json::json!({}),
            source_run_id: None,
            source_proposal_id: None,
            created_at: 1,
        }];

        let prompt = build_prompt_input(
            session_id,
            &observations,
            3_600,
            None,
            &[],
            &[],
            &rejections,
            &cfg(),
        );

        assert!(
            prompt
                .prompt
                .contains("Recent rejected auto-improvement attempts")
        );
        assert!(prompt.prompt.contains("notes/repeat.md"));
        assert!(prompt.prompt.contains("duplicate_existing_path"));
        assert!(prompt.prompt.contains("abc123"));
    }

    #[test]
    fn prompt_bounds_rejection_context_and_charges_observation_budget() {
        let session_id = SessionId::new();
        let observations: Vec<_> = (0..80)
            .map(|i| {
                obs(
                    i,
                    ObservationKind::PostToolUse,
                    &format!("obs {i}"),
                    "important bounded observation detail ".repeat(120).as_str(),
                    8,
                )
            })
            .collect();
        let rejections: Vec<_> = (0..20)
            .map(|i| AutoImproveRejectionSummary {
                id: format!("r{i}"),
                workspace_id: WorkspaceId::new(),
                project_id: ProjectId::new(),
                target_path: Some(format!("notes/{}repeat.md", "p".repeat(500))),
                kind: Some("note".into()),
                operation: Some("create".into()),
                edit_mode: Some("full_page".into()),
                reason: format!("duplicate_existing_path {}", "reason ".repeat(1_000)),
                normalized_fingerprint: "abcdef".repeat(80),
                summary: format!("Repeated oversized note {i}: {}", "summary ".repeat(2_000)),
                evidence_json: serde_json::json!({}),
                source_run_id: None,
                source_proposal_id: None,
                created_at: i,
            })
            .collect();
        let cfg = AutoImproveReviewConfig {
            max_input_tokens: 2_500,
            ..cfg()
        };

        let prompt = build_prompt_input(
            session_id,
            &observations,
            3_600,
            None,
            &[],
            &[],
            &rejections,
            &cfg,
        );

        let usable_chars = cfg
            .max_input_tokens
            .saturating_sub(PROMPT_RESERVE_TOKENS)
            .saturating_mul(CHARS_PER_TOKEN);
        let rejection_section = prompt
            .prompt
            .split("Recent rejected auto-improvement attempts for this workspace/project. Avoid repeating these target/reason/fingerprint patterns unless the session provides clearly new evidence:\n")
            .nth(1)
            .and_then(|rest| rest.split("\n\nConsolidated session page").next())
            .expect("prompt includes rejection section");
        assert!(rejection_section.len() <= rejection_context_char_budget(usable_chars));
        assert!(rejection_section.contains("[truncated]"));
        assert!(rejection_section.contains("[rejection context truncated]"));
        assert!(!prompt.prompt.contains(&"summary ".repeat(500)));
        assert!(prompt.prompt.contains("Selected observations"));
        assert!(
            prompt
                .rejected_candidates
                .iter()
                .any(|r| r.reason == "input_budget_sampled")
        );
        assert!(prompt.warnings.iter().any(|w| w.contains("sampled")));
    }

    #[test]
    fn validation_prepends_title_when_body_is_missing_h1() {
        let mut candidate = proposal("gotchas/missing-h1.md", "gotcha", 0.91);
        candidate.body_markdown = "Body without a heading.".into();
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![candidate],
            rejected_candidates: Vec::new(),
        };

        let (accepted, rejected, warnings) =
            validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert_eq!(accepted.len(), 1);
        assert!(rejected.is_empty());
        assert!(accepted[0].body_markdown.starts_with("# Test\n\n"));
        assert!(warnings.iter().any(|w| w.contains("prepended title")));
    }

    #[test]
    fn validation_rejects_existing_path_duplicates() {
        let existing = ExistingPageIndex::from_pages(
            &[BriefingPage {
                path: "gotchas/existing.md".into(),
                title: "Existing Lesson".into(),
                kind: "gotcha".into(),
                updated_at: "2026-06-15T00:00:00Z".into(),
            }],
            &[],
        );
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![
                proposal("gotchas/existing.md", "gotcha", 0.91),
                AutoImproveProposal {
                    title: "Existing Lesson".into(),
                    ..proposal("gotchas/same-title.md", "gotcha", 0.91)
                },
            ],
            rejected_candidates: Vec::new(),
        };

        let (accepted, rejected, _warnings) = validate_response(raw, &cfg(), &existing);
        assert!(accepted.is_empty());
        assert_eq!(rejected.len(), 2);
        assert_eq!(rejected[0].reason, "duplicate_existing_path");
        assert_eq!(rejected[1].reason, "duplicate_existing_title");
    }

    #[test]
    fn validation_rejects_low_confidence_and_bad_prefix() {
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![
                proposal("gotchas/good.md", "gotcha", 0.2),
                proposal("sessions/new.md", "fact", 0.9),
            ],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, _warnings) =
            validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert!(accepted.is_empty());
        assert_eq!(rejected.len(), 2);
        assert_eq!(rejected[0].reason, "confidence_below_threshold");
        assert_eq!(rejected[1].reason, "unsupported_path_prefix");
    }

    #[test]
    fn validation_caps_proposal_count() {
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![
                proposal("gotchas/one.md", "gotcha", 0.9),
                proposal("decisions/two.md", "decision", 0.9),
                proposal("concepts/three.md", "concept", 0.9),
            ],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, _warnings) =
            validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert_eq!(accepted.len(), 2);
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].reason, "max_proposals_exceeded");
    }

    #[test]
    fn patch_run_edit_budget_rejects_later_patches_only() {
        let body = "# Release\n\n## Steps\nOne\n";
        let mut first = patch_proposal("append", "## Steps", "Two");
        first.path = "procedures/release.md".into();
        let mut second = patch_proposal("append", "## Steps", "Three");
        second.path = "procedures/release.md".into();
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![first, second],
            rejected_candidates: Vec::new(),
        };
        let review_cfg = AutoImproveReviewConfig {
            max_patch_edits_per_run: 1,
            max_proposals_per_run: 3,
            ..cfg()
        };

        let (accepted, rejected, _) = validate_response(
            raw,
            &review_cfg,
            &patch_index("procedures/release.md", body),
        );
        assert_eq!(accepted.len(), 1);
        assert!(accepted[0].body_markdown.contains("Two"));
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].reason, "patch_run_edit_budget_exceeded");
    }

    #[test]
    fn rule_and_procedure_full_page_budgets_reject_only_targeted_kinds() {
        let mut rule = proposal("_rules/small.md", "rule", 0.91);
        rule.body_markdown = "# Small\n\nThis rule is too long for one token.".into();
        let mut procedure = proposal("procedures/small.md", "procedure", 0.91);
        procedure.body_markdown = "# Small\n\nThis procedure is too long for one token.".into();
        let mut gotcha = proposal("gotchas/unchanged.md", "gotcha", 0.91);
        gotcha.body_markdown = "# Small\n\nThis gotcha may exceed scoped page budgets.".into();
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![rule, procedure, gotcha],
            rejected_candidates: Vec::new(),
        };
        let review_cfg = AutoImproveReviewConfig {
            max_proposals_per_run: 3,
            max_rule_page_tokens: 1,
            max_procedure_page_tokens: 1,
            ..cfg()
        };

        let (accepted, rejected, _) =
            validate_response(raw, &review_cfg, &ExistingPageIndex::default());
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].path, "gotchas/unchanged.md");
        assert_eq!(rejected.len(), 2);
        assert_eq!(rejected[0].reason, "rule_page_budget_exceeded");
        assert_eq!(rejected[1].reason, "procedure_page_budget_exceeded");
    }

    #[test]
    fn rule_and_procedure_patch_materialized_bodies_obey_page_budgets() {
        let rule_body = "# Rule\n\n## Details\nShort\n";
        let procedure_body = "# Procedure\n\n## Steps\nShort\n";
        let mut rule_patch =
            patch_proposal("append", "## Details", "This makes the rule too long.");
        rule_patch.path = "_rules/small.md".into();
        rule_patch.kind = "rule".into();
        let mut procedure_patch =
            patch_proposal("append", "## Steps", "This makes the procedure too long.");
        procedure_patch.path = "procedures/small.md".into();
        let raw_rule = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![rule_patch],
            rejected_candidates: Vec::new(),
        };
        let raw_procedure = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![procedure_patch],
            rejected_candidates: Vec::new(),
        };
        let review_cfg = AutoImproveReviewConfig {
            max_rule_page_tokens: 2,
            max_procedure_page_tokens: 2,
            ..cfg()
        };

        let (accepted, rejected, _) = validate_response(
            raw_rule,
            &review_cfg,
            &patch_index("_rules/small.md", rule_body),
        );
        assert!(accepted.is_empty());
        assert_eq!(rejected[0].reason, "rule_page_budget_exceeded");

        let (accepted, rejected, _) = validate_response(
            raw_procedure,
            &review_cfg,
            &patch_index("procedures/small.md", procedure_body),
        );
        assert!(accepted.is_empty());
        assert_eq!(rejected[0].reason, "procedure_page_budget_exceeded");
    }

    fn patch_index(path: &str, body: &str) -> ExistingPageIndex {
        let kind = if path.starts_with("_rules/") {
            "rule"
        } else {
            "procedure"
        };
        ExistingPageIndex::from_pages(
            &[BriefingPage {
                path: path.into(),
                title: "Patch Target".into(),
                kind: kind.into(),
                updated_at: "2026-06-15T00:00:00Z".into(),
            }],
            &[patchable_context(path, "Patch Target", kind, body)],
        )
    }

    fn patchable_context(path: &str, title: &str, kind: &str, body: &str) -> PatchablePageContext {
        PatchablePageContext {
            path: path.into(),
            title: title.into(),
            kind: kind.into(),
            body: body.into(),
            body_sha256: sha256_hex(body.as_bytes()),
            sections: parse_markdown_sections(body),
        }
    }

    fn patch_proposal(op: &str, anchor: &str, content: &str) -> AutoImproveProposal {
        AutoImproveProposal {
            edit_mode: "patch".into(),
            edits: vec![AutoImprovePatchEdit {
                op: op.into(),
                anchor: anchor.into(),
                content: content.into(),
                section_sha256: None,
                context: None,
            }],
            body_markdown: String::new(),
            ..proposal("procedures/release.md", "procedure", 0.91)
        }
    }

    #[test]
    fn patch_to_missing_or_non_context_target_rejects() {
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![patch_proposal("append", "## Steps", "More")],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, _) = validate_response(raw, &cfg(), &ExistingPageIndex::default());
        assert!(accepted.is_empty());
        assert_eq!(rejected[0].reason, "patch_target_not_in_context");
    }

    #[test]
    fn patch_omitted_by_patchable_budget_rejects() {
        let included_body = "# Included\n\n## Steps\nOne\n";
        let omitted_body = "# Omitted\n\n## Steps\nOne\n";
        let included = patchable_context(
            "procedures/included.md",
            "Included",
            "procedure",
            included_body,
        );
        let omitted = patchable_context(
            "procedures/omitted.md",
            "Omitted",
            "procedure",
            omitted_body,
        );
        let mut warnings = Vec::new();
        let first_only = render_patchable_pages(
            std::slice::from_ref(&included),
            usize::MAX,
            usize::MAX,
            &mut warnings,
        );
        let rendered = render_patchable_pages(
            &[included.clone(), omitted.clone()],
            usize::MAX,
            first_only.text.len(),
            &mut warnings,
        );
        assert!(rendered.included_paths.contains("procedures/included.md"));
        assert!(!rendered.included_paths.contains("procedures/omitted.md"));

        let recent = [
            BriefingPage {
                path: "procedures/included.md".into(),
                title: "Included".into(),
                kind: "procedure".into(),
                updated_at: "2026-06-15T00:00:00Z".into(),
            },
            BriefingPage {
                path: "procedures/omitted.md".into(),
                title: "Omitted".into(),
                kind: "procedure".into(),
                updated_at: "2026-06-15T00:00:00Z".into(),
            },
        ];
        let prompt_patchable: Vec<_> = [included, omitted]
            .into_iter()
            .filter(|page| rendered.included_paths.contains(&page.path))
            .collect();
        let index = ExistingPageIndex::from_pages(&recent, &prompt_patchable);
        let mut candidate = patch_proposal("append", "## Steps", "More");
        candidate.path = "procedures/omitted.md".into();
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![candidate],
            rejected_candidates: Vec::new(),
        };

        let (accepted, rejected, _) = validate_response(raw, &cfg(), &index);
        assert!(accepted.is_empty());
        assert_eq!(rejected[0].reason, "patch_target_not_in_context");
    }

    #[test]
    fn max_patchable_pages_zero_rejects_patch_even_for_recent_rule_or_procedure() {
        let recent = [
            BriefingPage {
                path: "_rules/zero.md".into(),
                title: "Zero Rule".into(),
                kind: "rule".into(),
                updated_at: "2026-06-15T00:00:00Z".into(),
            },
            BriefingPage {
                path: "procedures/zero.md".into(),
                title: "Zero Procedure".into(),
                kind: "procedure".into(),
                updated_at: "2026-06-15T00:00:00Z".into(),
            },
        ];
        let index = ExistingPageIndex::from_pages(&recent, &[]);
        let mut rule_patch = patch_proposal("append", "## Steps", "More");
        rule_patch.path = "_rules/zero.md".into();
        rule_patch.kind = "rule".into();
        let mut procedure_patch = patch_proposal("append", "## Steps", "More");
        procedure_patch.path = "procedures/zero.md".into();
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![rule_patch, procedure_patch],
            rejected_candidates: Vec::new(),
        };

        let (accepted, rejected, _) = validate_response(raw, &cfg(), &index);
        assert!(accepted.is_empty());
        assert_eq!(rejected.len(), 2);
        assert!(
            rejected
                .iter()
                .all(|r| r.reason == "patch_target_not_in_context")
        );
    }

    #[test]
    fn duplicate_anchor_rejects() {
        let body = "# Release\n\n## Steps\nOne\n\n## Steps\nTwo\n";
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![patch_proposal("append", "## Steps", "More")],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, _) =
            validate_response(raw, &cfg(), &patch_index("procedures/release.md", body));
        assert!(accepted.is_empty());
        assert_eq!(rejected[0].reason, "duplicate_anchor");
    }

    #[test]
    fn append_add_section_and_replace_section_materialize() {
        let body = "# Release\n\n## Steps\nOne\n\n## Verify\nCheck\n";
        let section_hash = parse_markdown_sections(body)
            .into_iter()
            .find(|s| s.anchor == "## Verify")
            .unwrap()
            .sha256;
        let mut candidate = patch_proposal("append", "## Steps", "Two");
        candidate.edits.push(AutoImprovePatchEdit {
            op: "add_section".into(),
            anchor: "## Steps".into(),
            content: "## Rollback\nUndo".into(),
            section_sha256: None,
            context: None,
        });
        candidate.edits.push(AutoImprovePatchEdit {
            op: "replace_section".into(),
            anchor: "## Verify".into(),
            content: "## Verify\nRun tests".into(),
            section_sha256: Some(section_hash),
            context: None,
        });
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![candidate],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, _) =
            validate_response(raw, &cfg(), &patch_index("procedures/release.md", body));
        assert!(rejected.is_empty());
        let materialized = &accepted[0].body_markdown;
        assert!(materialized.contains("One"));
        assert!(materialized.contains("Two"));
        assert!(materialized.contains("## Rollback\nUndo"));
        assert!(materialized.contains("## Verify\nRun tests"));
        assert_eq!(
            accepted[0].expected_base_body_sha256.as_deref(),
            Some(sha256_hex(body.as_bytes()).as_str())
        );
    }

    #[test]
    fn replace_section_requires_matching_hash_and_delete_rejects() {
        let body = "# Release\n\n## Steps\nOne\n";
        let no_hash = patch_proposal("replace_section", "## Steps", "## Steps\nTwo");
        let mut wrong_hash = no_hash.clone();
        wrong_hash.edits[0].section_sha256 = Some("00".repeat(32));
        let delete = patch_proposal("delete_section", "## Steps", "");
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![no_hash, wrong_hash, delete],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, _) =
            validate_response(raw, &cfg(), &patch_index("procedures/release.md", body));
        assert!(accepted.is_empty());
        assert!(
            rejected
                .iter()
                .any(|r| r.reason == "replace_section_missing_hash")
        );
        assert!(
            rejected
                .iter()
                .any(|r| r.reason == "replace_section_hash_mismatch")
        );
        assert!(rejected.iter().any(|r| r.reason == "unsupported_patch_op"));
    }

    #[test]
    fn replace_section_requires_same_anchor_heading() {
        let body = "# Release\n\n## Steps\nOne\n\n## Verify\nCheck\n";
        let section_hash = parse_markdown_sections(body)
            .into_iter()
            .find(|s| s.anchor == "## Steps")
            .unwrap()
            .sha256;

        let mut empty = patch_proposal("replace_section", "## Steps", "");
        empty.edits[0].section_sha256 = Some(section_hash.clone());
        let mut prose = patch_proposal("replace_section", "## Steps", "Updated prose only");
        prose.edits[0].section_sha256 = Some(section_hash.clone());
        let mut renamed = patch_proposal("replace_section", "## Steps", "## Other\nUpdated");
        renamed.edits[0].section_sha256 = Some(section_hash);
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![empty, prose, renamed],
            rejected_candidates: Vec::new(),
        };

        let (accepted, rejected, _) =
            validate_response(raw, &cfg(), &patch_index("procedures/release.md", body));
        assert!(accepted.is_empty());
        assert!(
            rejected
                .iter()
                .any(|r| r.reason == "replace_section_missing_heading")
        );
        assert_eq!(
            rejected
                .iter()
                .filter(|r| r.reason == "replace_section_anchor_mismatch")
                .count(),
            2
        );
    }

    #[test]
    fn patch_boundaries_preserve_following_heading() {
        let body = "# Release\n\n## Steps\nOne\n\n## Verify\nCheck\n";
        let verify_hash = parse_markdown_sections(body)
            .into_iter()
            .find(|s| s.anchor == "## Steps")
            .unwrap()
            .sha256;
        let mut replace = patch_proposal("replace_section", "## Steps", "## Steps\nOne\nTwo");
        replace.edits[0].section_sha256 = Some(verify_hash);
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![replace],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, _) =
            validate_response(raw, &cfg(), &patch_index("procedures/release.md", body));
        assert!(rejected.is_empty());
        assert!(accepted[0].body_markdown.contains("Two\n\n## Verify"));

        let add = patch_proposal("add_section", "## Steps", "## Rollback\nUndo");
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![add],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, _) =
            validate_response(raw, &cfg(), &patch_index("procedures/release.md", body));
        assert!(rejected.is_empty());
        assert!(
            accepted[0]
                .body_markdown
                .contains("## Rollback\nUndo\n\n## Verify")
        );
    }

    #[test]
    fn add_section_requires_non_h1_sibling_heading() {
        let body = "# Release\n\n## Steps\nOne\n";
        let missing = patch_proposal("add_section", "## Steps", "No heading");
        let h1 = patch_proposal("add_section", "## Steps", "# Bad\nNope");
        let child = patch_proposal("add_section", "## Steps", "### Child\nNope");
        let prose_before_heading = patch_proposal(
            "add_section",
            "## Steps",
            "Introductory prose.\n\n## Rollback\nUndo",
        );
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![missing, h1, child, prose_before_heading],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, _) =
            validate_response(raw, &cfg(), &patch_index("procedures/release.md", body));
        assert!(accepted.is_empty());
        assert!(
            rejected
                .iter()
                .any(|r| r.reason == "add_section_missing_heading")
        );
        assert_eq!(
            rejected
                .iter()
                .filter(|r| r.reason == "add_section_missing_heading")
                .count(),
            2
        );
        assert!(
            rejected
                .iter()
                .any(|r| r.reason == "add_section_h1_unsupported")
        );
        assert!(
            rejected
                .iter()
                .any(|r| r.reason == "add_section_heading_level_mismatch")
        );
    }

    #[test]
    fn heading_parser_ignores_fenced_code_headings() {
        let body = "# Release\n\n```\n## Not Anchor\n```\n\n## Steps\nOne\n";
        let sections = parse_markdown_sections(body);
        assert!(sections.iter().any(|s| s.anchor == "## Steps"));
        assert!(!sections.iter().any(|s| s.anchor == "## Not Anchor"));
    }

    #[test]
    fn patch_rejects_duplicate_headings_and_extra_h1_introduced_by_edits() {
        let body = "# Release\n\n## Steps\nOne\n";
        let duplicate = patch_proposal("append", "## Steps", "## Steps\nAgain");
        let extra_h1 = patch_proposal("append", "## Steps", "# Extra\nNope");
        let raw = AutoImproveLlmResponse {
            summary: "ok".into(),
            proposals: vec![duplicate, extra_h1],
            rejected_candidates: Vec::new(),
        };
        let (accepted, rejected, _) =
            validate_response(raw, &cfg(), &patch_index("procedures/release.md", body));
        assert!(accepted.is_empty());
        assert!(rejected.iter().any(|r| r.reason == "duplicate_anchor"));
        assert!(rejected.iter().any(|r| r.reason == "patch_extra_h1"));
    }
}
