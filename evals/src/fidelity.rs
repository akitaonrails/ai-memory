//! Zero-LLM literal-fidelity metrics for the consolidation prompt (#1215).
//!
//! Two questions, kept apart so a loss is blamed on the right stage:
//!
//! - **projection**: did a fact reach the user message at all? The
//!   observation projection omits whole observations and truncates long
//!   bodies to fit the budget; a fact it drops is invisible to any model.
//! - **model**: of the facts that reached the prompt, which ones made it
//!   onto the written pages?
//!
//! The primary measure is the fixture's `must_mention` list: literal strings
//! chosen by the fixture author, independent of any token miner, so a
//! mechanism that adds tokens to the prompt cannot grade itself. The
//! `mine_keep_tokens` recall is a secondary, broader view and is reported as
//! such. Matching is exact, case-sensitive substring: a paraphrase counts as
//! a loss by definition, which is what "verbatim" means in the prompts.

use std::collections::BTreeSet;

use ai_memory_consolidate::{
    ConsolidatedBatch, build_batch_request, mine_keep_tokens, mine_residue_tokens,
};
use ai_memory_core::{Observation, SessionId};
use ai_memory_llm::ChatRequest;
use serde::Serialize;

/// What the projection let through to the user message.
#[derive(Debug, Clone, Serialize)]
pub struct PromptFidelity {
    pub observations: usize,
    /// Observation blocks rendered into the prompt.
    pub selected: usize,
    /// Rendered bodies cut at the per-body excerpt cap.
    pub truncated_bodies: usize,
    pub must_mention_total: usize,
    /// `must_mention` strings absent from the prompt.
    pub lost_in_projection: Vec<String>,
    /// Share of `must_mention` the projection dropped; `None` without facts.
    pub projection_loss: Option<f64>,
    /// Keep-token recall of the prompt (secondary, see the module docs).
    pub recall_prompt: Option<f64>,
    /// Forbidden fixture strings that reached the prompt.
    pub must_not_mention_hits: Vec<String>,
    /// Omitted/truncated-observation error-line coverage by the whole request.
    pub error_line_observation_coverage: Option<f64>,
    /// Share of recalled error-line tokens originating in file-family tools.
    pub file_error_line_share: Option<f64>,
}

/// What the model carried from the prompt onto its pages.
#[derive(Debug, Clone, Serialize)]
pub struct PageFidelity {
    /// `must_mention` strings that reached the prompt but not the pages.
    pub lost_by_model: Vec<String>,
    /// Share of the prompt-reaching facts the model dropped.
    pub llm_loss: Option<f64>,
    pub recall_page: Option<f64>,
    /// Keep-tokens on a page found in neither the request nor the batch's
    /// own paths: invented identifiers, paths, URLs.
    pub hallucinated_tokens: Vec<String>,
    /// `[[wikilinks]]` to pages outside this batch. The harness lists no
    /// existing pages in the prompt, so every such target is invented (#1207).
    pub invented_wikilinks: Vec<String>,
    pub must_not_mention_hits: Vec<String>,
}

/// The request and the inputs both sides of a fixture are scored against.
pub struct Scorer<'a> {
    must_mention: &'a [String],
    must_not_mention: &'a [String],
    request_text: String,
    observation_text: String,
    tokens: BTreeSet<String>,
    error_observations_total: usize,
    error_observations_recalled: usize,
    error_tokens_recalled: usize,
    file_error_tokens_recalled: usize,
}

impl<'a> Scorer<'a> {
    pub fn new(
        session_id: SessionId,
        observations: &[Observation],
        request: &ChatRequest,
        must_mention: &'a [String],
        must_not_mention: &'a [String],
    ) -> Self {
        // Tokens the scaffold always carries would count as recalled whatever
        // the projection did, so they are not part of the denominator.
        let scaffold = request_text(&build_batch_request(session_id, &[], false));
        let mut observation_text = String::new();
        let mut tokens = BTreeSet::new();
        for obs in observations {
            let text = format!("{}\n{}\n", obs.title, obs.body);
            tokens.extend(
                mine_keep_tokens(&text)
                    .into_iter()
                    .filter(|t| !scaffold.contains(t.as_str())),
            );
            observation_text.push_str(&text);
        }
        let request_text = request_text(request);
        let mut error_observations_total = 0usize;
        let mut error_observations_recalled = 0usize;
        let mut error_tokens_recalled = 0usize;
        let mut file_error_tokens_recalled = 0usize;
        for observation in observations {
            let error_tokens: Vec<String> = mine_residue_tokens(&observation.body, usize::MAX)
                .into_iter()
                .filter(|token| looks_like_error_line(token))
                .collect();
            if error_tokens.is_empty() {
                continue;
            }
            error_observations_total += 1;
            let recalled: Vec<&String> = error_tokens
                .iter()
                .filter(|token| request_text.contains(token.as_str()))
                .collect();
            if !recalled.is_empty() {
                error_observations_recalled += 1;
            }
            error_tokens_recalled += recalled.len();
            if observation
                .body
                .lines()
                .next()
                .is_some_and(|line| line.trim() == "tool_family: file")
            {
                file_error_tokens_recalled += recalled.len();
            }
        }
        Self {
            must_mention,
            must_not_mention,
            request_text,
            observation_text,
            tokens,
            error_observations_total,
            error_observations_recalled,
            error_tokens_recalled,
            file_error_tokens_recalled,
        }
    }

    pub fn prompt(&self, observations: usize) -> PromptFidelity {
        let lost: Vec<String> = self
            .must_mention
            .iter()
            .filter(|f| !self.request_text.contains(f.as_str()))
            .cloned()
            .collect();
        PromptFidelity {
            observations,
            selected: self.request_text.matches("\n--- observation ").count(),
            truncated_bodies: self
                .request_text
                .matches("[observation body truncated;")
                .count(),
            must_mention_total: self.must_mention.len(),
            projection_loss: share(lost.len(), self.must_mention.len()),
            lost_in_projection: lost,
            recall_prompt: recall(&self.tokens, &self.request_text),
            must_not_mention_hits: self
                .must_not_mention
                .iter()
                .filter(|value| self.request_text.contains(value.as_str()))
                .cloned()
                .collect(),
            error_line_observation_coverage: share(
                self.error_observations_recalled,
                self.error_observations_total,
            ),
            file_error_line_share: share(
                self.file_error_tokens_recalled,
                self.error_tokens_recalled,
            ),
        }
    }

    pub fn page(&self, batch: &ConsolidatedBatch) -> PageFidelity {
        let page_text = page_text(batch);
        let in_prompt: Vec<&String> = self
            .must_mention
            .iter()
            .filter(|f| self.request_text.contains(f.as_str()))
            .collect();
        let lost_by_model: Vec<String> = in_prompt
            .iter()
            .filter(|f| !page_text.contains(f.as_str()))
            .map(|f| (*f).clone())
            .collect();
        let own_paths: Vec<&str> = batch.updates.iter().map(|u| u.path.as_str()).collect();
        let mut hallucinated = BTreeSet::new();
        for update in &batch.updates {
            for token in mine_keep_tokens(&update.body_markdown) {
                let known = self.request_text.contains(&token)
                    || self.observation_text.contains(&token)
                    || own_paths.iter().any(|p| p.contains(&token));
                if !known {
                    hallucinated.insert(token);
                }
            }
        }
        // The miner yields both `a/b/c.rs` and `c.rs`; one invention counts once.
        let hallucinated_tokens: Vec<String> = hallucinated
            .iter()
            .filter(|t| {
                !hallucinated
                    .iter()
                    .any(|o| o != *t && o.contains(t.as_str()))
            })
            .cloned()
            .collect();
        PageFidelity {
            llm_loss: share(lost_by_model.len(), in_prompt.len()),
            lost_by_model,
            recall_page: recall(&self.tokens, &page_text),
            hallucinated_tokens,
            invented_wikilinks: invented_wikilinks(&page_text, &own_paths),
            must_not_mention_hits: self
                .must_not_mention
                .iter()
                .filter(|s| page_text.contains(s.as_str()))
                .cloned()
                .collect(),
        }
    }
}

fn looks_like_error_line(token: &str) -> bool {
    let first = token.split_whitespace().next().unwrap_or_default();
    [
        "error",
        "ERROR:",
        "ERRO:",
        "Erro:",
        "ERR!",
        "panicked at ",
        "FAILED",
        "fatal:",
        "FATAL:",
        "Traceback (most recent call last)",
    ]
    .iter()
    .any(|prefix| token.starts_with(prefix))
        || first.trim_end_matches(':').strip_suffix("Error").is_some()
        || first
            .trim_end_matches(':')
            .strip_suffix("Exception")
            .is_some()
}

/// System prompt plus every message, as the provider receives them.
pub fn request_text(request: &ChatRequest) -> String {
    let mut out = request.system.clone().unwrap_or_default();
    for message in &request.messages {
        out.push('\n');
        out.push_str(&message.content);
    }
    out
}

fn page_text(batch: &ConsolidatedBatch) -> String {
    let mut out = String::new();
    for update in &batch.updates {
        out.push_str(&update.title);
        out.push('\n');
        out.push_str(&update.body_markdown);
        out.push('\n');
    }
    out
}

fn recall(tokens: &BTreeSet<String>, text: &str) -> Option<f64> {
    share(
        tokens.iter().filter(|t| text.contains(t.as_str())).count(),
        tokens.len(),
    )
}

fn share(part: usize, whole: usize) -> Option<f64> {
    (whole > 0).then(|| part as f64 / whole as f64)
}

fn invented_wikilinks(page_text: &str, own_paths: &[&str]) -> Vec<String> {
    let own: Vec<&str> = own_paths
        .iter()
        .map(|p| p.strip_suffix(".md").unwrap_or(p))
        .collect();
    let mut out = BTreeSet::new();
    for chunk in page_text.split("[[").skip(1) {
        let Some((inner, _)) = chunk.split_once("]]") else {
            continue;
        };
        let target = inner.split(['|', '#']).next().unwrap_or("").trim();
        let target = target.strip_suffix(".md").unwrap_or(target);
        if !target.is_empty() && !own.contains(&target) {
            out.insert(target.to_string());
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_consolidate::ConsolidatedPageUpdate;
    use ai_memory_core::{ObservationId, ObservationKind, ProjectId, WorkspaceId};
    use jiff::Timestamp;

    fn obs(session_id: SessionId, body: &str) -> Observation {
        Observation {
            id: ObservationId::new(),
            session_id,
            workspace_id: WorkspaceId::new(),
            project_id: ProjectId::new(),
            kind: ObservationKind::UserPrompt,
            extension: None,
            source_event: None,
            title: "user prompt".into(),
            body: body.into(),
            importance: 5,
            created_at: Timestamp::now(),
            trust_class: ai_memory_core::TrustClass::OperatorAuthored,
        }
    }

    fn update(path: &str, body: &str) -> ConsolidatedPageUpdate {
        serde_json::from_value(serde_json::json!({
            "path": path,
            "title": "t",
            "body_markdown": body,
            "tier": "episodic",
            "kind": "fact",
            "tags": [],
            "entities": [],
        }))
        .unwrap()
    }

    fn batch(updates: Vec<ConsolidatedPageUpdate>) -> ConsolidatedBatch {
        ConsolidatedBatch {
            updates,
            rationale: String::new(),
        }
    }

    #[test]
    fn a_fact_cut_by_the_body_cap_is_lost_in_projection_not_by_the_model() {
        let sid = SessionId::new();
        // The fact sits past the 3,000-character excerpt cap.
        let body = format!("{} run backfill-verify.sh at the end", "x".repeat(3_100));
        let observations = vec![obs(sid, "keep 97 MB/s"), obs(sid, &body)];
        let request = build_batch_request(sid, &observations, false);
        let facts = vec!["97 MB/s".to_string(), "backfill-verify.sh".to_string()];
        let scorer = Scorer::new(sid, &observations, &request, &facts, &[]);

        let prompt = scorer.prompt(observations.len());
        assert_eq!(prompt.lost_in_projection, vec!["backfill-verify.sh"]);
        assert_eq!(prompt.projection_loss, Some(0.5));
        assert_eq!(prompt.truncated_bodies, 1);
        assert_eq!(prompt.selected, 2);

        // The model kept everything it saw: no model loss, even though one
        // fact never reached it.
        let page = scorer.page(&batch(vec![update("sessions/x.md", "rate 97 MB/s")]));
        assert!(page.lost_by_model.is_empty());
        assert_eq!(page.llm_loss, Some(0.0));

        // Control: a page that drops the visible fact is charged to the model.
        let page = scorer.page(&batch(vec![update("sessions/x.md", "fast")]));
        assert_eq!(page.lost_by_model, vec!["97 MB/s"]);
        assert_eq!(page.llm_loss, Some(1.0));
    }

    #[test]
    fn invented_tokens_links_and_forbidden_strings_are_reported() {
        let sid = SessionId::new();
        let observations = vec![obs(sid, "edited crates/store/src/fence.rs")];
        let request = build_batch_request(sid, &observations, false);
        let forbidden = vec!["ghp_FAKE0123456789ABCDEFGHIJ".to_string()];
        let scorer = Scorer::new(sid, &observations, &request, &[], &forbidden);
        assert!(
            scorer
                .prompt(observations.len())
                .must_not_mention_hits
                .is_empty()
        );
        let page = scorer.page(&batch(vec![
            update(
                "concepts/fence.md",
                "Changed crates/store/src/fence.rs and crates/store/src/ghost.rs. \
                 See [[concepts/fence]], [[decisions/never-written|why]]. \
                 Token ghp_FAKE0123456789ABCDEFGHIJ.",
            ),
            update("sessions/x.md", "see `concepts/fence.md`"),
        ]));
        assert_eq!(page.hallucinated_tokens, vec!["crates/store/src/ghost.rs"]);
        assert_eq!(page.invented_wikilinks, vec!["decisions/never-written"]);
        assert_eq!(page.must_not_mention_hits, forbidden);
    }

    #[test]
    fn scaffold_tokens_stay_out_of_the_recall_denominator() {
        let sid = SessionId::new();
        // `body_markdown` is in the batch scaffold, so it can never be lost.
        let observations = vec![obs(sid, "body_markdown and fence_epoch")];
        let request = build_batch_request(sid, &observations, false);
        let scorer = Scorer::new(sid, &observations, &request, &[], &[]);
        assert_eq!(scorer.tokens, BTreeSet::from(["fence_epoch".to_string()]));
        let page = scorer.page(&batch(vec![update("sessions/x.md", "nothing")]));
        assert_eq!(page.recall_page, Some(0.0));
    }
}
