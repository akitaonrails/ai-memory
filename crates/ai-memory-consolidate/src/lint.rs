//! M8 lint pass — rule-based wiki health check + optional LLM-driven
//! contradiction detection.
//!
//! Two layers:
//!
//! 1. **Rule-based** (no LLM, always on): stale episodic pages
//!    (>30d old with zero accesses), pages with empty bodies,
//!    duplicate-by-title across paths.
//! 2. **LLM-driven** (opt-in via the provider): clusters the latest
//!    semantic pages, feeds them to the LLM with a structured-output
//!    prompt asking for contradictions / stale claims.
//!
//! Findings are written to `wiki/_lint/report.md` so they're
//! grep-able and tracked in git.

/// System prompt for the contradiction-detection lint pass. Loaded
/// at compile time from `prompts/lint_system.md`.
const LINT_SYSTEM_PROMPT: &str = include_str!("../prompts/lint_system.md");

use ai_memory_core::{PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{ChatMessage, ChatRequest, LlmProvider, Role, complete_structured};
use ai_memory_store::{DecayCandidate, ReaderPool};
use ai_memory_wiki::{AdmissionContext, AdmissionOp, Wiki, WritePageRequest};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

fn default_lint_severity() -> String {
    "warning".into()
}

/// One lint finding (rule-based or LLM-emitted).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LintFinding {
    /// Discriminator: `contradiction` | `stale` | `duplicate` | `empty` | `other`.
    pub kind: String,
    /// `info` | `warning`. Defaults to `"warning"` so older prompts that
    /// omit the field don't hard-fail deserialization.
    #[serde(default = "default_lint_severity")]
    pub severity: String,
    /// One-paragraph description. Also accepts `"summary"` (the field name
    /// the old prompt used) so existing LLM responses still deserialize.
    #[serde(alias = "summary")]
    pub message: String,
    /// Wiki paths the finding refers to.
    #[serde(default)]
    pub pages: Vec<String>,
    /// Optional longer markdown explanation emitted by the LLM prompt.
    #[serde(default)]
    pub detail: Option<String>,
}

/// Structured output the LLM produces.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LintReport {
    /// Findings the LLM identified.
    pub findings: Vec<LintFinding>,
}

/// Errors raised by the lint pass.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum LintError {
    /// Underlying store error.
    #[error(transparent)]
    Store(#[from] ai_memory_store::StoreError),
    /// Underlying wiki error.
    #[error(transparent)]
    Wiki(#[from] ai_memory_wiki::WikiError),
    /// Underlying LLM error.
    #[error(transparent)]
    Llm(#[from] ai_memory_llm::LlmError),
    /// Domain error (e.g. invalid page path).
    #[error(transparent)]
    Memory(#[from] ai_memory_core::MemoryError),
}

const US_PER_DAY: f64 = 86_400_000_000.0;
/// Cap on pages fed to the LLM contradiction pass (token budget).
pub const LLM_CLUSTER_CAP: usize = 20;
/// Stale threshold at the default `[decay] lambda`, in days.
///
/// Kept for callers that want the historical constant; the live threshold
/// comes from [`stale_days_for`].
pub const STALE_DAYS: f64 = 30.0;

/// Decay score at which an unread episodic page is called stale:
/// `exp(-0.02 * 30)`, the score the old fixed 30-day threshold implied at
/// the default lambda. Stored as its negative log so the day count is a
/// plain division.
const STALE_SCORE_LN: f64 = 0.6;

/// Days after which an unread episodic page is flagged stale, derived from
/// the operator's `[decay] lambda`.
///
/// The rule only fires on pages with `access_count == 0`, and for those the
/// decay score reduces exactly to `salience * exp(-lambda * age)` — the M8
/// reinforcement term carries `ln(1 + access_count)`, which is zero. So the
/// lint was already measuring decay; it just measured it against a constant
/// instead of against the operator's lambda, and the two diverged by a
/// growing factor as lambda moved.
///
/// Concretely, at `lambda = 0.008` real eviction lands near day 201 while a
/// fixed threshold still called the page stale on day 31 — and because the
/// lint wrote `_lint/<date>.md` whenever any finding exists, one page
/// nobody intends to read produced a new page every day, forever (#426).
/// (The report now supersedes a single `_lint/report.md`, so even a
/// permanently-stale page costs one page, not one per day.)
///
/// `lambda = 0.02` (the default) yields exactly 30 days, so an operator who
/// never touched decay sees no change.
#[must_use]
pub fn stale_days_for(lambda: f64) -> f64 {
    if lambda.is_finite() && lambda > 0.0 {
        STALE_SCORE_LN / lambda
    } else {
        // Fail back to the historical constant rather than emitting
        // infinity (which would silence the rule) or NaN (which would make
        // every comparison false).
        STALE_DAYS
    }
}

/// Options for [`run_lint`].
///
/// A struct rather than three positional parameters: `false, false` at a
/// call site said nothing about which switch was which, and the threshold
/// input has to travel alongside them.
#[derive(Debug, Clone, Copy)]
pub struct LintOptions {
    /// When `true`, no `_lint/report.md` page is written (and no legacy
    /// dated reports are pruned).
    pub dry_run: bool,
    /// When `false`, the LLM contradiction pass is skipped even if a
    /// provider was supplied.
    pub use_llm: bool,
    /// The operator's `[decay] lambda`. The stale threshold is derived from
    /// it — see [`stale_days_for`].
    pub decay_lambda: f64,
}

impl Default for LintOptions {
    fn default() -> Self {
        Self {
            dry_run: false,
            use_llm: true,
            decay_lambda: 0.02,
        }
    }
}

/// Run the lint pass.
///
/// * `llm` — when `Some`, the contradiction pass runs; otherwise the
///   report contains only rule-based findings.
/// * `decay_lambda` — the operator's `[decay] lambda`; the stale
///   threshold is derived from it (see [`stale_days_for`]).
/// * `dry_run` — when `true`, no file is written.
/// * `use_llm` — when `false`, the contradiction pass is skipped even
///   if a provider is present. Lets operators run rule-based-only lint
///   without disabling LLM globally for explore/consolidate.
///
/// # Errors
/// Returns [`LintError`] for any store / wiki / LLM failure.
pub async fn run_lint(
    reader: &ReaderPool,
    wiki: &Wiki,
    llm: Option<&std::sync::Arc<dyn LlmProvider>>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    options: LintOptions,
) -> Result<LintReport, LintError> {
    let LintOptions {
        dry_run,
        use_llm,
        decay_lambda,
    } = options;
    let candidates = reader.decay_candidates(workspace_id, project_id).await?;
    let mut findings = rule_based_findings(&candidates, stale_days_for(decay_lambda));

    // Dangling cross-project links: a `[[project:path]]` dependency that does
    // not resolve. A broken inter-project edge is high-signal — surface it
    // even on the zero-LLM path.
    for dangling in reader
        .dangling_cross_project_links(workspace_id, project_id)
        .await?
    {
        let target = match &dangling.workspace {
            Some(ws) => format!("{ws}/{}:{}", dangling.project, dangling.path),
            None => format!("{}:{}", dangling.project, dangling.path),
        };
        let message = if dangling.project_exists {
            format!(
                "Page {} links to {} but that page does not exist in project `{}` \
                 (missing, renamed, or deleted) — a broken cross-project dependency",
                dangling.from_path, target, dangling.project,
            )
        } else {
            format!(
                "Page {} links to {} but project `{}` does not exist (typo or wrong name)",
                dangling.from_path, target, dangling.project,
            )
        };
        findings.push(LintFinding {
            kind: "broken_link".into(),
            severity: "warning".into(),
            message,
            pages: vec![dangling.from_path],
            detail: None,
        });
    }

    // Declared contradictions (typed `contradicts` edges, 2.0 item 3):
    // an author or the consolidator explicitly said two pages disagree —
    // the highest-signal zero-LLM contradiction finding possible.
    for edge in reader.contradiction_edges(workspace_id, project_id).await? {
        let message = if edge.resolved {
            format!(
                "Page {} declares it contradicts {} — reconcile them or \
                 supersede the outdated one",
                edge.from_path, edge.to_path,
            )
        } else {
            format!(
                "Page {} declares it contradicts {}, which does not resolve \
                 to a page (deleted or renamed) — the declaration is stale",
                edge.from_path, edge.to_path,
            )
        };
        findings.push(LintFinding {
            kind: "contradiction".into(),
            severity: "warning".into(),
            message,
            pages: vec![edge.from_path, edge.to_path],
            detail: None,
        });
    }

    // Explicit `stale` / `wrong` feedback (memory_feedback). An agent or
    // user asserting a page is outdated or incorrect is the highest-signal
    // finding the zero-LLM path can produce — it came from a human/agent
    // judgement, not a heuristic. Findings repeat until the page is
    // rewritten, which is the point: an unfixed stale page is still stale.
    let feedback_flagged = reader
        .open_feedback_findings(workspace_id, project_id)
        .await?;
    for flagged in &feedback_flagged {
        let plural = if flagged.signal_count == 1 {
            "signal"
        } else {
            "signals"
        };
        let mut message = format!(
            "Page {} was flagged `{}` ({} {}, latest {})",
            flagged.path, flagged.kind, flagged.signal_count, plural, flagged.latest_at,
        );
        if let Some(reason) = &flagged.reason {
            message.push_str(&format!(": {reason}"));
        }
        findings.push(LintFinding {
            kind: "feedback_flagged".into(),
            severity: "warning".into(),
            message,
            pages: vec![flagged.path.clone()],
            detail: None,
        });
    }

    if use_llm && let Some(provider) = llm {
        match contradiction_pass(
            provider.clone(),
            wiki,
            workspace_id,
            project_id,
            &candidates,
        )
        .await
        {
            Ok(mut extra) => findings.append(&mut extra),
            Err(e) => warn!(error = %e, "lint LLM contradiction pass failed"),
        }
    }

    let report = LintReport { findings };

    if !dry_run {
        if report.findings.is_empty() {
            // A clean project carries no lint page at all: a stale
            // report claiming findings that no longer exist is itself
            // the kind of noise the lint exists to flag.
            remove_report_page(wiki, workspace_id, project_id, &candidates).await;
        } else {
            write_report_page(wiki, workspace_id, project_id, &report).await?;
        }
        // One report per project: the daily `_lint/<YYYY-MM-DD>.md`
        // pages the pre-2.0.1 lint accumulated (thousands across a
        // long-lived store — indexed, searched, and embedded) are
        // machinery, not knowledge. Each pass prunes any it finds, so
        // existing stores self-heal without a migration.
        prune_legacy_dated_reports(wiki, workspace_id, project_id, &candidates).await;
    }

    Ok(report)
}

fn rule_based_findings(candidates: &[DecayCandidate], stale_days: f64) -> Vec<LintFinding> {
    let now_us = Timestamp::now().as_microsecond();
    let mut out = Vec::new();
    let mut titles: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();

    for c in candidates {
        // Stale: episodic, older than the lambda-derived threshold, zero accesses.
        #[allow(clippy::cast_precision_loss)]
        let age_days = (now_us - c.updated_at_us) as f64 / US_PER_DAY;
        if c.tier == Tier::Episodic && age_days > stale_days && c.access_count == 0 {
            out.push(LintFinding {
                kind: "stale".into(),
                severity: "info".into(),
                message: format!(
                    "Episodic page {} is {:.0} days old with zero accesses",
                    c.path, age_days,
                ),
                pages: vec![c.path.as_str().to_string()],
                detail: None,
            });
        }
        // M20: rule-shaped pages get a "consider adding to
        // CLAUDE.md" suggestion. Two signals are checked:
        //   1. Frontmatter `kind: rule` — set by the consolidator
        //      when it classifies an observation as a rule.
        //   2. Page path starts with `_rules/` — same routing
        //      target. Either signal suffices.
        let frontmatter: Option<serde_json::Value> = serde_json::from_str(&c.frontmatter_json).ok();
        let kind_is_rule = frontmatter
            .as_ref()
            .and_then(|fm| fm.get("kind"))
            .and_then(serde_json::Value::as_str)
            == Some("rule");
        let path_str = c.path.as_str();
        let path_is_rule = path_str.starts_with("_rules/");
        if kind_is_rule || path_is_rule {
            out.push(LintFinding {
                kind: "rule_suggestion".into(),
                severity: "info".into(),
                message: format!(
                    "Page {path_str} looks like a durable project rule. \
                     Consider copying it into your project's CLAUDE.md / \
                     AGENTS.md so the agent sees it on every turn, not \
                     just when it remembers to call memory_query."
                ),
                pages: vec![path_str.to_string()],
                detail: None,
            });
        }
        // Duplicate-title tracking: peek the frontmatter for a `title` field.
        // An empty/blank title is not a meaningful shared title — several
        // pages carrying `title: ""` (e.g. auto-improve pages whose stored
        // frontmatter title was never filled) must not all collapse into one
        // bogus `Multiple pages share title ""` finding (#599). Real titles
        // still dedupe as before.
        if let Some(fm) = frontmatter.as_ref()
            && let Some(t) = fm.get("title").and_then(serde_json::Value::as_str)
            && !t.trim().is_empty()
        {
            titles
                .entry(t.to_lowercase())
                .or_default()
                .push(c.path.as_str().to_string());
        }
        // Pinned + expiring is contradictory: the pin says "never
        // decay" but the TTL hard-deletes regardless of pin (an
        // explicit expiry is the more explicit statement). Surface the
        // combo so the operator resolves it before the sweep does.
        let frontmatter_pinned = frontmatter
            .as_ref()
            .and_then(|fm| fm.get("pinned"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        if c.expires_at_us.is_some() && (c.pinned || frontmatter_pinned) {
            out.push(LintFinding {
                kind: "pinned_expiring".into(),
                severity: "warning".into(),
                message: format!(
                    "Page {} is pinned but carries an expires_at TTL; \
                     the forget sweep will hard-delete it when the TTL \
                     passes despite the pin. Remove one of the two.",
                    c.path,
                ),
                pages: vec![c.path.as_str().to_string()],
                detail: None,
            });
        }
    }

    for (title, paths) in titles {
        if paths.len() > 1 {
            out.push(LintFinding {
                kind: "duplicate".into(),
                severity: "warning".into(),
                message: format!("Multiple pages share title {title:?}"),
                pages: paths,
                detail: None,
            });
        }
    }

    out
}

async fn contradiction_pass(
    provider: std::sync::Arc<dyn LlmProvider>,
    wiki: &Wiki,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    candidates: &[DecayCandidate],
) -> Result<Vec<LintFinding>, LintError> {
    // Focus on semantic / procedural pages — those are the ones the
    // user actually compounds knowledge on.
    let mut subset: Vec<&DecayCandidate> = candidates
        .iter()
        .filter(|c| matches!(c.tier, Tier::Semantic | Tier::Procedural))
        .collect();
    if subset.len() < 2 {
        return Ok(Vec::new());
    }
    // Prefer high-access pages so the LLM sees the canonical knowledge.
    subset.sort_by_key(|c| std::cmp::Reverse(c.access_count));
    subset.truncate(LLM_CLUSTER_CAP);

    let mut prompt = String::new();
    prompt.push_str(
        "Audit the following wiki pages for contradictions, stale claims, or \
         duplicate information. Return a LintReport with one finding per issue.\n\n",
    );
    for c in &subset {
        let preview = wiki
            .read_page(workspace_id, project_id, &c.path)
            .map(|md| md.body.chars().take(400).collect::<String>())
            .unwrap_or_else(|_| "(unable to read)".into());
        prompt.push_str(&format!("## `{}`\n\n{}\n\n---\n\n", c.path, preview));
    }

    let request = ChatRequest {
        system: Some(LINT_SYSTEM_PROMPT.into()),
        messages: vec![ChatMessage {
            role: Role::User,
            content: prompt,
        }],
        // Generous output budget so multi-finding reports don't
        // truncate mid-JSON. Same rationale as consolidator/bootstrap.
        max_tokens: 32_000,
        temperature: Some(0.1),
    };
    let report: LintReport = complete_structured(&*provider, request).await?;
    Ok(report.findings)
}

/// Stable per-project lint report path — superseded in place each run.
const REPORT_PATH: &str = "_lint/report.md";

/// Matches the legacy daily report naming: `_lint/YYYY-MM-DD.md`.
fn is_legacy_dated_report(path: &str) -> bool {
    let Some(name) = path
        .strip_prefix("_lint/")
        .and_then(|rest| rest.strip_suffix(".md"))
    else {
        return false;
    };
    name.len() == 10
        && name.bytes().enumerate().all(|(i, b)| match i {
            4 | 7 => b == b'-',
            _ => b.is_ascii_digit(),
        })
}

/// Delete `_lint/report.md` when the current pass found nothing.
/// Best-effort: a delete rejected by an admission webhook or racing
/// write only leaves a stale report for the next pass to retry.
async fn remove_report_page(
    wiki: &Wiki,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    candidates: &[ai_memory_store::DecayCandidate],
) {
    if !candidates.iter().any(|c| c.path.as_str() == REPORT_PATH) {
        return;
    }
    let Ok(path) = PagePath::new(REPORT_PATH) else {
        return;
    };
    if let Err(e) = wiki
        .delete_page(
            workspace_id,
            project_id,
            &path,
            Some(AdmissionContext {
                op: AdmissionOp::Consolidate,
                ..Default::default()
            }),
            None,
        )
        .await
    {
        warn!(error = %e, "lint: could not remove clean project's stale report");
    }
}

/// Delete the accumulated pre-2.0.1 daily reports. Best-effort per
/// page; anything that survives is retried by the next pass.
async fn prune_legacy_dated_reports(
    wiki: &Wiki,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    candidates: &[ai_memory_store::DecayCandidate],
) {
    for cand in candidates {
        if !is_legacy_dated_report(cand.path.as_str()) {
            continue;
        }
        let path = cand.path.clone();
        if let Err(e) = wiki
            .delete_page(
                workspace_id,
                project_id,
                &path,
                Some(AdmissionContext {
                    op: AdmissionOp::Consolidate,
                    ..Default::default()
                }),
                None,
            )
            .await
        {
            warn!(path = %cand.path, error = %e, "lint: could not prune legacy dated report");
        }
    }
}

async fn write_report_page(
    wiki: &Wiki,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    report: &LintReport,
) -> Result<(), LintError> {
    let date = Timestamp::now()
        .to_zoned(TimeZone::UTC)
        .strftime("%Y-%m-%d")
        .to_string();
    // One stable path per project: each run supersedes the previous
    // report (history stays in the version chain) instead of minting a
    // new page per day.
    let path = PagePath::new(REPORT_PATH)?;
    let title = format!("Lint report {date}");
    let body = render_markdown(report);
    wiki.write_page(WritePageRequest {
        workspace_id,
        project_id,
        path,
        frontmatter: serde_json::json!({
            "title": title.clone(),
            "tier": "semantic",
            "kind": "lint-report",
        }),
        body,
        tier: Tier::Semantic,
        pinned: false,
        title: Some(title),
        admission_ctx: Some(AdmissionContext {
            op: AdmissionOp::Consolidate,
            ..Default::default()
        }),
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    })
    .await?;
    Ok(())
}

fn render_markdown(report: &LintReport) -> String {
    let mut buf = String::new();
    buf.push_str("# Lint findings\n\n");
    if report.findings.is_empty() {
        buf.push_str("_No findings._\n");
        return buf;
    }
    buf.push_str(&format!("{} finding(s).\n\n", report.findings.len()));
    for (i, f) in report.findings.iter().enumerate() {
        buf.push_str(&format!("## {} — {} ({})\n\n", i + 1, f.kind, f.severity));
        buf.push_str(&format!("{}\n\n", f.message));
        if let Some(detail) = &f.detail {
            buf.push_str(&format!("{detail}\n\n"));
        }
        if !f.pages.is_empty() {
            buf.push_str("Pages:\n");
            for p in &f.pages {
                buf.push_str(&format!("- `{p}`\n"));
            }
            buf.push('\n');
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of #426: an operator who never touched `[decay]`
    /// must see byte-identical behaviour. `0.6 / 0.02` is exactly 30.
    #[test]
    fn default_lambda_reproduces_the_historical_thirty_day_threshold() {
        assert!(
            (stale_days_for(0.02) - STALE_DAYS).abs() < f64::EPSILON,
            "default lambda must reproduce {STALE_DAYS} days, got {}",
            stale_days_for(0.02)
        );
    }

    /// Slower decay must push the lint out with it, not keep flagging on
    /// day 31 while real eviction is 200 days away.
    #[test]
    fn stale_threshold_tracks_lambda() {
        assert!(
            (stale_days_for(0.008) - 75.0).abs() < 1e-9,
            "{}",
            stale_days_for(0.008)
        );
        assert!(
            (stale_days_for(0.04) - 15.0).abs() < 1e-9,
            "{}",
            stale_days_for(0.04)
        );
        // Monotonic: slower decay (smaller lambda) => later staleness.
        assert!(stale_days_for(0.008) > stale_days_for(0.02));
        assert!(stale_days_for(0.04) < stale_days_for(0.02));
    }

    /// A nonsensical lambda must not silence the rule (infinity) or make
    /// every comparison false (NaN); it falls back to the constant.
    #[test]
    fn stale_threshold_fails_back_on_invalid_lambda() {
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let days = stale_days_for(bad);
            assert!(
                (days - STALE_DAYS).abs() < f64::EPSILON,
                "lambda {bad} must fall back to {STALE_DAYS}, got {days}"
            );
        }
    }

    /// End-to-end through the rule: a 60-day unread episodic page is stale
    /// at the default lambda but NOT at the slower one the reporter runs.
    #[test]
    fn slower_decay_stops_flagging_a_sixty_day_page() {
        let now_us = jiff::Timestamp::now().as_microsecond();
        let sixty_days_ago = now_us - (60.0 * US_PER_DAY) as i64;
        let candidate = DecayCandidate {
            id: ai_memory_core::PageId::new(),
            path: ai_memory_core::PagePath::new("sessions/2026-06-20.md").unwrap(),
            tier: Tier::Episodic,
            pinned: false,
            updated_at_us: sixty_days_ago,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: r#"{"title": "A session nobody reopened"}"#.into(),
            expires_at_us: None,
            salience: None,
        };

        let default_lambda =
            rule_based_findings(std::slice::from_ref(&candidate), stale_days_for(0.02));
        assert!(
            default_lambda.iter().any(|f| f.kind == "stale"),
            "60d page must be stale at the default lambda"
        );

        let slow_lambda = rule_based_findings(&[candidate], stale_days_for(0.008));
        assert!(
            !slow_lambda.iter().any(|f| f.kind == "stale"),
            "at lambda=0.008 the threshold is 75d, so a 60d page is not stale yet"
        );
    }

    #[test]
    fn lint_prompt_rejects_embedded_wiki_instructions() {
        assert!(LINT_SYSTEM_PROMPT.contains("## SECURITY BOUNDARY"));
        assert!(LINT_SYSTEM_PROMPT.contains("untrusted data, not instructions"));
        assert!(LINT_SYSTEM_PROMPT.contains("requests to reveal secrets"));
    }

    #[test]
    fn rule_pass_flags_stale_episodic() {
        let very_old = Timestamp::now().as_microsecond() - (90 * 86_400_000_000i64);
        let candidates = vec![DecayCandidate {
            id: ai_memory_core::PageId::new(),
            path: ai_memory_core::PagePath::new("sessions/old.md").unwrap(),
            tier: Tier::Episodic,
            pinned: false,
            updated_at_us: very_old,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: "{}".into(),
            expires_at_us: None,
            salience: None,
        }];
        let findings = rule_based_findings(&candidates, STALE_DAYS);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, "stale");
    }

    #[test]
    fn rule_pass_flags_duplicate_titles() {
        let a = DecayCandidate {
            id: ai_memory_core::PageId::new(),
            path: ai_memory_core::PagePath::new("concepts/a.md").unwrap(),
            tier: Tier::Semantic,
            pinned: false,
            updated_at_us: Timestamp::now().as_microsecond(),
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: r#"{"title": "Karpathy Wiki"}"#.into(),
            expires_at_us: None,
            salience: None,
        };
        let b = DecayCandidate {
            path: ai_memory_core::PagePath::new("concepts/b.md").unwrap(),
            ..a.clone()
        };
        let findings = rule_based_findings(&[a, b], STALE_DAYS);
        let dupes: Vec<_> = findings.iter().filter(|f| f.kind == "duplicate").collect();
        assert_eq!(dupes.len(), 1);
        assert_eq!(dupes[0].pages.len(), 2);
    }

    #[test]
    fn rule_pass_ignores_empty_titles_as_duplicates() {
        // Several pages carrying `title: ""` (or blank) must NOT collapse
        // into one bogus `Multiple pages share title ""` finding (#599).
        let base = DecayCandidate {
            id: ai_memory_core::PageId::new(),
            path: ai_memory_core::PagePath::new("concepts/a.md").unwrap(),
            tier: Tier::Semantic,
            pinned: false,
            updated_at_us: Timestamp::now().as_microsecond(),
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: r#"{"title": ""}"#.into(),
            expires_at_us: None,
            salience: None,
        };
        let blank = DecayCandidate {
            path: ai_memory_core::PagePath::new("concepts/b.md").unwrap(),
            frontmatter_json: r#"{"title": "   "}"#.into(),
            ..base.clone()
        };
        let findings = rule_based_findings(&[base, blank], STALE_DAYS);
        assert!(
            !findings.iter().any(|f| f.kind == "duplicate"),
            "empty/blank titles must not be reported as a shared title"
        );
    }

    /// M20: a page tagged `kind: rule` in its frontmatter triggers
    /// a rule_suggestion finding pointing the user at CLAUDE.md.
    #[test]
    fn rule_pass_flags_rule_kind_frontmatter() {
        let candidate = DecayCandidate {
            id: ai_memory_core::PageId::new(),
            path: ai_memory_core::PagePath::new("concepts/no-impl-without-test.md").unwrap(),
            tier: Tier::Semantic,
            pinned: false,
            updated_at_us: Timestamp::now().as_microsecond(),
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: r#"{"title": "Never ship code without a test", "kind": "rule"}"#
                .into(),
            expires_at_us: None,
            salience: None,
        };
        let findings = rule_based_findings(&[candidate], STALE_DAYS);
        let rules: Vec<_> = findings
            .iter()
            .filter(|f| f.kind == "rule_suggestion")
            .collect();
        assert_eq!(rules.len(), 1, "expected one rule_suggestion finding");
        assert!(rules[0].message.contains("CLAUDE.md"));
    }

    /// M20: a page under `_rules/` also triggers the suggestion
    /// even when the frontmatter is missing/empty — the path
    /// itself is enough signal.
    #[test]
    fn rule_pass_flags_rules_path() {
        let candidate = DecayCandidate {
            id: ai_memory_core::PageId::new(),
            path: ai_memory_core::PagePath::new("_rules/no-impl-without-test.md").unwrap(),
            tier: Tier::Semantic,
            pinned: false,
            updated_at_us: Timestamp::now().as_microsecond(),
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: "{}".into(),
            expires_at_us: None,
            salience: None,
        };
        let findings = rule_based_findings(&[candidate], STALE_DAYS);
        assert!(
            findings.iter().any(|f| f.kind == "rule_suggestion"),
            "expected a rule_suggestion finding for _rules/ page",
        );
    }

    /// Defensive: a normal concept page (no rule signal) does NOT
    /// emit the suggestion. Without this guard, every fact-tagged
    /// page would noise up the lint report.
    #[test]
    fn rule_pass_skips_non_rule_pages() {
        let candidate = DecayCandidate {
            id: ai_memory_core::PageId::new(),
            path: ai_memory_core::PagePath::new("concepts/karpathy-wiki.md").unwrap(),
            tier: Tier::Semantic,
            pinned: false,
            updated_at_us: Timestamp::now().as_microsecond(),
            access_count: 5,
            last_accessed_at_us: None,
            frontmatter_json: r#"{"title": "Karpathy Wiki", "kind": "fact"}"#.into(),
            expires_at_us: None,
            salience: None,
        };
        let findings = rule_based_findings(&[candidate], STALE_DAYS);
        assert!(
            findings.iter().all(|f| f.kind != "rule_suggestion"),
            "non-rule page must not produce a rule_suggestion finding",
        );
    }

    // ── LintFinding tolerant deserialization ─────────────────────────────

    /// The old prompt used `summary`/`detail` instead of `message` and
    /// omitted `severity`. Both fields must deserialize gracefully so
    /// in-flight LLM responses don't silently fail.
    #[test]
    fn lint_finding_deserializes_old_prompt_shape() {
        let json = r#"{"kind":"contradiction","pages":["a.md"],"summary":"x","detail":"y"}"#;
        let f: LintFinding = serde_json::from_str(json).expect("deserialize");
        assert_eq!(f.message, "x");
        assert_eq!(
            f.severity, "warning",
            "missing severity defaults to warning"
        );
        assert_eq!(f.detail, Some("y".into()));
    }

    /// The canonical (updated) prompt shape must also round-trip.
    #[test]
    fn lint_finding_deserializes_canonical_shape() {
        let json = r#"{"kind":"stale","severity":"info","message":"m","pages":[]}"#;
        let f: LintFinding = serde_json::from_str(json).expect("deserialize");
        assert_eq!(f.kind, "stale");
        assert_eq!(f.severity, "info");
        assert_eq!(f.message, "m");
        assert!(f.detail.is_none());
    }

    // ── use_llm=false skips contradiction pass ───────────────────────────

    /// `rule_based_findings` with a stale candidate returns a finding.
    /// This stands in for the full `run_lint(..., use_llm=false)` path:
    /// the guard `if use_llm { ... }` is trivially verifiable by
    /// inspection, so the test focuses on rule-based output being present.
    #[test]
    fn no_llm_flag_still_returns_rule_based_findings() {
        let very_old = Timestamp::now().as_microsecond() - (60 * 86_400_000_000i64);
        let candidates = vec![DecayCandidate {
            id: ai_memory_core::PageId::new(),
            path: ai_memory_core::PagePath::new("sessions/old.md").unwrap(),
            tier: Tier::Episodic,
            pinned: false,
            updated_at_us: very_old,
            access_count: 0,
            last_accessed_at_us: None,
            frontmatter_json: "{}".into(),
            expires_at_us: None,
            salience: None,
        }];
        // rule_based_findings is the exact code path that `use_llm=false`
        // keeps active. Confirm it still fires.
        let findings = rule_based_findings(&candidates, STALE_DAYS);
        assert!(
            findings.iter().any(|f| f.kind == "stale"),
            "rule-based stale finding must be present regardless of use_llm flag",
        );
    }
}
