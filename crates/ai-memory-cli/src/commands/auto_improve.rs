//! `ai-memory auto-improve` — review one session and apply durable wiki edits through the auto-improvement approval path.

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::cli::AutoImproveArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/auto-improve`.
#[derive(Serialize)]
struct AutoImproveRequest {
    workspace: String,
    project: String,
    session_id: String,
    min_observations: usize,
    min_session_duration_secs: u64,
    min_confidence: f32,
    max_input_tokens: usize,
    max_proposals_per_run: usize,
    max_patchable_pages: usize,
    max_patchable_body_chars: usize,
    max_edits_per_proposal: usize,
    max_edit_content_chars: usize,
    max_changed_chars_per_proposal: usize,
    max_patch_edits_per_run: usize,
    max_rejection_context: usize,
    rejection_context_days: u32,
    max_final_body_chars: usize,
    max_rule_page_tokens: usize,
    max_procedure_page_tokens: usize,
    include_raw_fallback: bool,
    proposal_actor: String,
    pending_path: String,
    eval: ai_memory_consolidate::AutoImproveEvalConfig,
}

#[derive(Debug, Deserialize, Serialize)]
struct StageResponse {
    run_id: String,
    approval_required: bool,
    approval_policy: String,
    session_id: String,
    summary: String,
    warnings: Vec<String>,
    rejected_candidates_count: usize,
    proposals: Vec<ProposalOutcome>,
}

#[derive(Debug, Deserialize, Serialize)]
struct ProposalOutcome {
    id: String,
    sidecar_path: String,
    status: String,
    page_id: Option<String>,
}

/// Run the `auto-improve` subcommand.
///
/// # Errors
/// Returns an error if the server is unreachable or rejects the review request.
pub async fn run(config: &Config, args: AutoImproveArgs) -> Result<()> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let settings = &config.auto_improve;
    let request = AutoImproveRequest {
        workspace,
        project: project.clone(),
        session_id: args.session_id,
        min_observations: args.min_observations.unwrap_or(settings.min_observations),
        min_session_duration_secs: args
            .min_session_duration_secs
            .unwrap_or(settings.min_session_duration_secs),
        min_confidence: args.min_confidence.unwrap_or(settings.min_confidence),
        max_input_tokens: args.max_input_tokens.unwrap_or(settings.max_input_tokens),
        max_proposals_per_run: args.max_proposals.unwrap_or(settings.max_proposals_per_run),
        max_patchable_pages: settings.max_patchable_pages,
        max_patchable_body_chars: settings.max_patchable_body_chars,
        max_edits_per_proposal: settings.max_edits_per_proposal,
        max_edit_content_chars: settings.max_edit_content_chars,
        max_changed_chars_per_proposal: settings.max_changed_chars_per_proposal,
        max_patch_edits_per_run: settings.max_patch_edits_per_run,
        max_rejection_context: settings.max_rejection_context,
        rejection_context_days: settings.rejection_context_days,
        max_final_body_chars: settings.max_final_body_chars,
        max_rule_page_tokens: settings.max_rule_page_tokens,
        max_procedure_page_tokens: settings.max_procedure_page_tokens,
        include_raw_fallback: args.include_raw_fallback || settings.include_raw_fallback,
        proposal_actor: settings.proposal_actor.clone(),
        pending_path: settings.pending_path.clone(),
        eval: ai_memory_consolidate::AutoImproveEvalConfig {
            enabled: settings.eval.enabled,
            command: settings.eval.command.clone(),
            timeout_secs: settings.eval.timeout_secs,
            targets: settings.eval.targets.clone(),
            min_delta: settings.eval.min_delta,
        },
    };

    let response: StageResponse = post_json(&endpoint, "/admin/auto-improve", &request).await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        if response.approval_required {
            println!(
                "Staged auto-improve run {} for manual approval",
                response.run_id
            );
        } else {
            println!("Auto-approved auto-improve run {}", response.run_id);
        }
        println!("Approval policy: {}", response.approval_policy);
        for proposal in &response.proposals {
            if let Some(page_id) = &proposal.page_id {
                println!(
                    "  - {} [{}] page {} ({})",
                    proposal.id, proposal.status, page_id, proposal.sidecar_path
                );
            } else {
                println!(
                    "  - {} [{}] ({})",
                    proposal.id, proposal.status, proposal.sidecar_path
                );
            }
        }
    }
    Ok(())
}
