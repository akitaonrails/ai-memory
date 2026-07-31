//! `ai-memory auto-improve` — review one session and apply durable wiki edits through the auto-improvement approval path.

use ai_memory_store::SkippedProposal;
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
    /// Proposals the server staged nothing for, with the reason. Re-declared
    /// here because `--json` prints THIS struct, not the server's body: a key
    /// missing from it is dropped by serde and never reaches the operator.
    /// `default` keeps an older server (which sends no such key) parsing.
    #[serde(default)]
    skipped: Vec<SkippedProposal>,
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
        println!("{}", render_human(&response));
    }
    Ok(())
}

fn render_human(response: &StageResponse) -> String {
    let mut lines = Vec::new();
    if response.approval_required {
        lines.push(format!(
            "Staged auto-improve run {} for manual approval",
            response.run_id
        ));
    } else {
        lines.push(format!(
            "Auto-approved auto-improve run {}",
            response.run_id
        ));
    }
    lines.push(format!("Approval policy: {}", response.approval_policy));
    for proposal in &response.proposals {
        if let Some(page_id) = &proposal.page_id {
            lines.push(format!(
                "  - {} [{}] page {} ({})",
                proposal.id, proposal.status, page_id, proposal.sidecar_path
            ));
        } else {
            lines.push(format!(
                "  - {} [{}] ({})",
                proposal.id, proposal.status, proposal.sidecar_path
            ));
        }
    }
    lines.extend(super::skipped_proposal_lines(&response.skipped));
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(skipped: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "run_id": "run-1",
            "approval_required": true,
            "approval_policy": "manual",
            "session_id": "sess-1",
            "summary": "one durable procedure",
            "warnings": [],
            "rejected_candidates_count": 0,
            "proposals": [],
            "skipped": skipped,
        })
    }

    #[test]
    fn skipped_survives_the_response_round_trip_into_json_output() {
        let response: StageResponse = serde_json::from_value(wire(serde_json::json!([{
            "target_path": "procedures/release.md",
            "reason": "a proposal is already pending review for this path",
        }])))
        .expect("server body parses");

        // `--json` prints this struct, so the assertion has to be on what the
        // struct serialises back to, not on what arrived.
        let printed = serde_json::to_value(&response).expect("re-serialise");
        assert_eq!(
            printed["skipped"][0]["target_path"], "procedures/release.md",
            "--json dropped the skipped proposals: {printed}"
        );
        assert_eq!(
            printed["skipped"][0]["reason"],
            "a proposal is already pending review for this path"
        );
    }

    #[test]
    fn skipped_reaches_human_output_too() {
        let response: StageResponse = serde_json::from_value(wire(serde_json::json!([{
            "target_path": "procedures/release.md",
            "reason": "a proposal is already pending review for this path",
        }])))
        .expect("server body parses");
        let human = render_human(&response);
        assert!(human.contains("procedures/release.md"), "{human}");
        assert!(human.contains("already pending review"), "{human}");
    }

    #[test]
    fn a_clean_run_prints_no_skipped_section() {
        let response: StageResponse =
            serde_json::from_value(wire(serde_json::json!([]))).expect("server body parses");
        assert!(!render_human(&response).contains("Skipped"));
    }

    #[test]
    fn a_server_without_the_field_still_parses() {
        let mut body = wire(serde_json::json!([]));
        body.as_object_mut().unwrap().remove("skipped");
        let response: StageResponse = serde_json::from_value(body).expect("older server body");
        assert!(response.skipped.is_empty());
    }
}
