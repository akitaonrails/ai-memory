//! Rule-based curator report for conservative wiki maintenance signals.

use std::collections::HashMap;

use ai_memory_core::{ProjectId, Tier, WorkspaceId};
use ai_memory_store::{DecayParams, ReaderPool, retention_score};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

const US_PER_DAY: f64 = 86_400_000_000.0;

/// Parameters for one curator report run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorParams {
    /// Maximum findings returned per signal class.
    pub max_findings_per_kind: usize,
    /// Age threshold for `_slots/current-focus.md`.
    pub current_focus_stale_days: f64,
    /// Age threshold for other `_slots/*` pages.
    pub other_slot_stale_days: f64,
    /// Retention parameters for cold episodic detection.
    pub decay_params: DecayParams,
}

impl Default for CuratorParams {
    fn default() -> Self {
        Self {
            max_findings_per_kind: 25,
            current_focus_stale_days: 7.0,
            other_slot_stale_days: 30.0,
            decay_params: DecayParams::default(),
        }
    }
}

/// One conservative maintenance signal found by the curator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorFinding {
    /// Signal kind (`cold_episodic`, `stale_slot`, `duplicate_title`, `dangling_cross_project_link`).
    pub kind: String,
    /// Human severity for UI/CLI rendering.
    pub severity: String,
    /// Human-readable finding text.
    pub message: String,
    /// Pages involved in the finding.
    pub pages: Vec<String>,
    /// Optional structured details.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
}

/// Structured curator report. It is report-only: approving a staged report
/// writes this report page and performs no maintenance actions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorReport {
    /// Workspace reviewed.
    pub workspace: String,
    /// Project reviewed.
    pub project: String,
    /// ISO timestamp of report generation.
    pub generated_at: String,
    /// True when returned from the dry-run path.
    pub dry_run: bool,
    /// Short summary.
    pub summary: String,
    /// Parameters used for this run.
    pub params: CuratorParams,
    /// Conservative findings.
    pub findings: Vec<CuratorFinding>,
}

/// Build a rule-based curator report. This function only reads store state.
///
/// # Errors
/// Propagates store read failures.
pub async fn run_curator_report(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    workspace_name: &str,
    project_name: &str,
    params: CuratorParams,
) -> ai_memory_store::StoreResult<CuratorReport> {
    let now = Timestamp::now();
    let now_us = now.as_microsecond();
    let mut findings = Vec::new();

    let candidates = reader.decay_candidates(workspace_id, project_id).await?;
    let mut cold = Vec::new();
    for c in &candidates {
        if c.tier != Tier::Episodic || c.pinned || frontmatter_pinned(&c.frontmatter_json) {
            continue;
        }
        let page_age_days = age_days(now_us, c.updated_at_us);
        let days_since_access = c.last_accessed_at_us.map(|us| age_days(now_us, us));
        let score = retention_score(
            &params.decay_params,
            page_age_days,
            c.access_count,
            days_since_access,
        );
        if score < params.decay_params.cold_threshold {
            cold.push(CuratorFinding {
                kind: "cold_episodic".into(),
                severity: "info".into(),
                message: format!(
                    "Episodic page {} is cold (score {:.3}, age {:.0} days)",
                    c.path.as_str(),
                    score,
                    page_age_days
                ),
                pages: vec![c.path.as_str().to_string()],
                detail: Some(serde_json::json!({
                    "score": score,
                    "threshold": params.decay_params.cold_threshold,
                    "age_days": page_age_days,
                    "access_count": c.access_count,
                })),
            });
        }
    }
    cold.sort_by(|a, b| a.message.cmp(&b.message));
    findings.extend(cold.into_iter().take(params.max_findings_per_kind));

    let pages = reader.list_pages(workspace_name, project_name).await?;
    let mut stale_slots = Vec::new();
    let mut titles: HashMap<String, Vec<String>> = HashMap::new();
    for page in &pages {
        if page.path.starts_with("_pending/") {
            continue;
        }
        if page.path.starts_with("_slots/")
            && let Ok(updated) = page.updated_at.parse::<Timestamp>()
        {
            let slot_age_days = age_days(now_us, updated.as_microsecond());
            let threshold = if page.path == "_slots/current-focus.md" {
                params.current_focus_stale_days
            } else {
                params.other_slot_stale_days
            };
            if slot_age_days > threshold {
                stale_slots.push(CuratorFinding {
                    kind: "stale_slot".into(),
                    severity: "warning".into(),
                    message: format!(
                        "Slot {} has not changed for {:.0} days (threshold {:.0})",
                        page.path, slot_age_days, threshold
                    ),
                    pages: vec![page.path.clone()],
                    detail: Some(serde_json::json!({
                        "age_days": slot_age_days,
                        "threshold_days": threshold,
                    })),
                });
            }
        }
        let title = normalize_title(&page.title);
        if !title.is_empty() {
            titles.entry(title).or_default().push(page.path.clone());
        }
    }
    stale_slots.sort_by(|a, b| a.pages.cmp(&b.pages));
    findings.extend(stale_slots.into_iter().take(params.max_findings_per_kind));

    let mut duplicate_titles = Vec::new();
    for (title, mut paths) in titles {
        if paths.len() > 1 {
            paths.sort();
            duplicate_titles.push(CuratorFinding {
                kind: "duplicate_title".into(),
                severity: "info".into(),
                message: format!("{} pages share the normalized title '{title}'", paths.len()),
                pages: paths,
                detail: Some(serde_json::json!({"normalized_title": title})),
            });
        }
    }
    duplicate_titles.sort_by(|a, b| a.message.cmp(&b.message));
    findings.extend(
        duplicate_titles
            .into_iter()
            .take(params.max_findings_per_kind),
    );

    let dangling = reader
        .dangling_cross_project_links(workspace_id, project_id)
        .await?;
    findings.extend(
        dangling
            .into_iter()
            .take(params.max_findings_per_kind)
            .map(|link| {
                let target = format!(
                    "{}/{}/{}",
                    link.workspace.as_deref().unwrap_or(workspace_name),
                    link.project,
                    link.path
                );
                CuratorFinding {
                    kind: "dangling_cross_project_link".into(),
                    severity: "warning".into(),
                    message: format!(
                        "{} links to missing cross-project target {target}",
                        link.from_path
                    ),
                    pages: vec![link.from_path],
                    detail: Some(serde_json::json!({
                        "target": target,
                        "project_exists": link.project_exists,
                    })),
                }
            }),
    );

    let summary = if findings.is_empty() {
        "No conservative curator findings.".to_string()
    } else {
        format!("{} conservative curator finding(s).", findings.len())
    };

    Ok(CuratorReport {
        workspace: workspace_name.to_string(),
        project: project_name.to_string(),
        generated_at: now.to_string(),
        dry_run: true,
        summary,
        params,
        findings,
    })
}

/// Render the report as a normal wiki markdown page.
#[must_use]
pub fn render_curator_report_markdown(report: &CuratorReport) -> String {
    let mut out = String::new();
    out.push_str("# Curator Report\n\n");
    out.push_str("> Report-only: approving this pending write stores this report page only. ");
    out.push_str("It does not edit, delete, merge, rewrite links, or update slots.\n\n");
    out.push_str(&format!("- Workspace: `{}`\n", report.workspace));
    out.push_str(&format!("- Project: `{}`\n", report.project));
    out.push_str(&format!("- Generated: `{}`\n", report.generated_at));
    out.push_str(&format!("- Summary: {}\n\n", report.summary));
    if report.findings.is_empty() {
        out.push_str("No conservative curator findings.\n");
        return out;
    }
    out.push_str("## Findings\n\n");
    for finding in &report.findings {
        out.push_str(&format!(
            "- **{}** ({}) — {}\n",
            finding.kind, finding.severity, finding.message
        ));
        if !finding.pages.is_empty() {
            out.push_str(&format!("  - Pages: `{}`\n", finding.pages.join("`, `")));
        }
    }
    out
}

fn frontmatter_pinned(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.get("pinned").and_then(serde_json::Value::as_bool))
        .unwrap_or(false)
}

fn normalize_title(title: &str) -> String {
    title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn age_days(now_us: i64, then_us: i64) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let age = now_us.saturating_sub(then_us) as f64 / US_PER_DAY;
    age.max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_core::{ActorContext, NewPage, PageId, PagePath};
    use ai_memory_store::Store;
    use ai_memory_wiki::{Wiki, WritePageRequest};
    use rusqlite::params;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    const DAY_US: i64 = 86_400_000_000;

    struct Fixture {
        tmp: TempDir,
        store: Store,
        wiki: Wiki,
        ws: WorkspaceId,
        proj: ProjectId,
    }

    async fn seed_fixture() -> Fixture {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
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
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        Fixture {
            tmp,
            store,
            wiki,
            ws,
            proj,
        }
    }

    /// Write one page through the normal wiki path (disk + store row).
    async fn write(
        fx: &Fixture,
        path: &str,
        title: &str,
        tier: Tier,
        pinned: bool,
        body: &str,
    ) -> PageId {
        fx.wiki
            .write_page(WritePageRequest {
                workspace_id: fx.ws,
                project_id: fx.proj,
                path: PagePath::new(path).unwrap(),
                frontmatter: serde_json::json!({"title": title}),
                body: body.to_string(),
                tier,
                pinned,
                title: Some(title.to_string()),
                admission_ctx: None,
                author_id: None,
                actor: ActorContext::anonymous(),
            })
            .await
            .unwrap()
    }

    /// Time-travel one page's timestamps via a secondary SQLite connection
    /// (same trick as `tests/lifecycle.rs`; WAL mode allows it while the
    /// writer actor is idle).
    fn backdate(fx: &Fixture, id: PageId, days: i64) {
        let db_path = fx.tmp.path().join("db/memory.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.pragma_update(None, "busy_timeout", 5_000).unwrap();
        let updated_at = Timestamp::now().as_microsecond() - days * DAY_US;
        conn.execute(
            "UPDATE pages SET created_at = ?1, updated_at = ?1 WHERE id = ?2",
            params![updated_at, id.as_bytes()],
        )
        .unwrap();
    }

    async fn run_report(fx: &Fixture) -> CuratorReport {
        run_curator_report(
            &fx.store.reader,
            fx.ws,
            fx.proj,
            "default",
            "scratch",
            CuratorParams::default(),
        )
        .await
        .unwrap()
    }

    fn findings_of_kind<'a>(report: &'a CuratorReport, kind: &str) -> Vec<&'a CuratorFinding> {
        report.findings.iter().filter(|f| f.kind == kind).collect()
    }

    #[test]
    fn age_days_handles_zero_exact_and_future_boundaries() {
        // 1.7e15 < 2^53, so these microsecond values are exact in f64.
        let now: i64 = 1_700_000_000_000_000;
        assert_eq!(age_days(now, now), 0.0, "same instant is zero days");
        assert_eq!(age_days(now, now - DAY_US), 1.0, "exactly one day");
        assert_eq!(age_days(now, now - 30 * DAY_US), 30.0, "exact threshold");
        assert_eq!(
            age_days(now, now + DAY_US),
            0.0,
            "future timestamps clamp to zero"
        );
        assert_eq!(
            age_days(0, i64::MAX),
            0.0,
            "saturating_sub avoids overflow on extreme values"
        );
        let half = age_days(now, now - DAY_US / 2);
        assert!((half - 0.5).abs() < 1e-9, "half a day: {half}");
    }

    #[test]
    fn normalize_title_collapses_whitespace_and_case() {
        assert_eq!(normalize_title("  Release   Notes "), "release notes");
        assert_eq!(normalize_title("Single"), "single");
        assert_eq!(normalize_title(""), "");
    }

    #[test]
    fn frontmatter_pinned_only_true_for_explicit_boolean() {
        assert!(frontmatter_pinned(r#"{"pinned": true}"#));
        assert!(!frontmatter_pinned(r#"{"pinned": false}"#));
        assert!(!frontmatter_pinned(r#"{"pinned": "true"}"#));
        assert!(!frontmatter_pinned("{}"));
        assert!(!frontmatter_pinned("not json"));
    }

    #[tokio::test]
    async fn classifies_cold_episodic_and_skips_warm_and_non_episodic() {
        let fx = seed_fixture().await;
        let cold = write(
            &fx,
            "sessions/old.md",
            "Old Session",
            Tier::Episodic,
            false,
            "old session body",
        )
        .await;
        write(
            &fx,
            "sessions/fresh.md",
            "Fresh Session",
            Tier::Episodic,
            false,
            "fresh body",
        )
        .await;
        let semantic = write(
            &fx,
            "concepts/old-concept.md",
            "Old Concept",
            Tier::Semantic,
            false,
            "old concept body",
        )
        .await;
        backdate(&fx, cold, 200);
        backdate(&fx, semantic, 200);

        let report = run_report(&fx).await;

        assert_eq!(
            report.findings.len(),
            1,
            "only the old unpinned episodic page is a finding: {:?}",
            report.findings
        );
        let finding = &report.findings[0];
        assert_eq!(finding.kind, "cold_episodic");
        assert_eq!(finding.severity, "info");
        assert_eq!(finding.pages, ["sessions/old.md"]);
        assert!(finding.message.contains("sessions/old.md"));
        let detail = finding.detail.as_ref().unwrap();
        let score = detail["score"].as_f64().unwrap();
        let threshold = detail["threshold"].as_f64().unwrap();
        assert!(
            score < threshold,
            "score {score} below threshold {threshold}"
        );
        assert!(detail["age_days"].as_f64().unwrap() >= 200.0);
        assert_eq!(detail["access_count"].as_i64().unwrap(), 0);
        assert_eq!(report.summary, "1 conservative curator finding(s).");
    }

    #[tokio::test]
    async fn pinned_pages_never_appear_as_cold_findings() {
        let fx = seed_fixture().await;
        // Column-pinned page, written through the normal wiki path.
        let pinned = write(
            &fx,
            "notes/pinned-old.md",
            "Pinned Old",
            Tier::Episodic,
            true,
            "pinned body",
        )
        .await;
        // Frontmatter-pinned only (column stays false) — the curator must
        // honor the frontmatter flag on its own. `Wiki::write_page` would
        // propagate frontmatter `pinned` into the column, so upsert the row
        // directly to isolate the frontmatter path.
        let fm_pinned = fx
            .store
            .writer
            .upsert_page(NewPage {
                workspace_id: fx.ws,
                project_id: fx.proj,
                path: PagePath::new("notes/fm-pinned.md").unwrap(),
                title: "FM Pinned".to_string(),
                body: "frontmatter-pinned body".to_string(),
                tier: Tier::Episodic,
                frontmatter_json: serde_json::json!({"title": "FM Pinned", "pinned": true}),
                pinned: false,
                links: Vec::new(),
                author_id: None,
            })
            .await
            .unwrap();
        backdate(&fx, pinned, 200);
        backdate(&fx, fm_pinned, 200);

        let report = run_report(&fx).await;

        assert!(
            report.findings.is_empty(),
            "pinned pages must never surface as findings: {:?}",
            report.findings
        );
        assert_eq!(report.summary, "No conservative curator findings.");
    }

    #[tokio::test]
    async fn classifies_stale_slots_with_per_slot_thresholds() {
        let fx = seed_fixture().await;
        // 8 days old: past the 7-day current-focus threshold…
        let focus = write(
            &fx,
            "_slots/current-focus.md",
            "Current Focus",
            Tier::Semantic,
            false,
            "focus body",
        )
        .await;
        // …but well under the 30-day threshold for other slots.
        let recent = write(
            &fx,
            "_slots/recent-notes.md",
            "Recent Notes",
            Tier::Semantic,
            false,
            "recent body",
        )
        .await;
        let old = write(
            &fx,
            "_slots/old-context.md",
            "Old Context",
            Tier::Semantic,
            false,
            "old body",
        )
        .await;
        backdate(&fx, focus, 8);
        backdate(&fx, recent, 8);
        backdate(&fx, old, 31);

        let report = run_report(&fx).await;

        let stale = findings_of_kind(&report, "stale_slot");
        assert_eq!(
            report.findings.len(),
            2,
            "exactly two stale slots, nothing else: {:?}",
            report.findings
        );
        // Stale findings sort by page path.
        assert_eq!(stale[0].pages, ["_slots/current-focus.md"]);
        assert_eq!(stale[0].severity, "warning");
        assert!(stale[0].message.contains("threshold 7"));
        let focus_detail = stale[0].detail.as_ref().unwrap();
        assert_eq!(focus_detail["threshold_days"].as_f64().unwrap(), 7.0);
        assert!(focus_detail["age_days"].as_f64().unwrap() >= 8.0);
        assert_eq!(stale[1].pages, ["_slots/old-context.md"]);
        let old_detail = stale[1].detail.as_ref().unwrap();
        assert_eq!(old_detail["threshold_days"].as_f64().unwrap(), 30.0);
        assert!(old_detail["age_days"].as_f64().unwrap() >= 31.0);
    }

    #[tokio::test]
    async fn under_threshold_slots_and_pending_pages_produce_no_findings() {
        let fx = seed_fixture().await;
        // 6 days is under the 7-day current-focus threshold (comparison is
        // a strict `>`, so under-threshold slots must stay quiet).
        let focus = write(
            &fx,
            "_slots/current-focus.md",
            "Current Focus",
            Tier::Semantic,
            false,
            "focus body",
        )
        .await;
        backdate(&fx, focus, 6);
        // A `_pending/` page sharing a normalized title with a real page
        // must not count toward duplicate_title findings.
        write(
            &fx,
            "notes/release.md",
            "Release Notes",
            Tier::Semantic,
            false,
            "release body",
        )
        .await;
        write(
            &fx,
            "_pending/draft.md",
            "release notes",
            Tier::Episodic,
            false,
            "draft body",
        )
        .await;

        let report = run_report(&fx).await;

        assert!(
            report.findings.is_empty(),
            "under-threshold slots and _pending/ pages are excluded: {:?}",
            report.findings
        );
    }

    #[tokio::test]
    async fn classifies_duplicate_titles_case_and_whitespace_insensitively() {
        let fx = seed_fixture().await;
        write(
            &fx,
            "notes/release-a.md",
            "Release Notes",
            Tier::Semantic,
            false,
            "body a",
        )
        .await;
        write(
            &fx,
            "decisions/release-b.md",
            "  release   notes ",
            Tier::Semantic,
            false,
            "body b",
        )
        .await;
        write(
            &fx,
            "notes/unique.md",
            "Unique Title",
            Tier::Semantic,
            false,
            "body c",
        )
        .await;

        let report = run_report(&fx).await;

        assert_eq!(
            report.findings.len(),
            1,
            "one duplicate-title finding only: {:?}",
            report.findings
        );
        let finding = &report.findings[0];
        assert_eq!(finding.kind, "duplicate_title");
        assert_eq!(finding.severity, "info");
        assert_eq!(
            finding.pages,
            ["decisions/release-b.md", "notes/release-a.md"],
            "pages are sorted"
        );
        assert!(finding.message.contains("2 pages share"));
        assert!(finding.message.contains("'release notes'"));
        assert_eq!(
            finding.detail.as_ref().unwrap()["normalized_title"],
            serde_json::json!("release notes")
        );
    }

    #[tokio::test]
    async fn classifies_dangling_cross_project_links() {
        let fx = seed_fixture().await;
        // "infra" exists as a project but lacks the target page; "ghost-project"
        // does not exist at all — the report must distinguish the two.
        fx.store
            .writer
            .get_or_create_project(fx.ws, "infra", None)
            .await
            .unwrap();
        write(
            &fx,
            "notes/links.md",
            "Links",
            Tier::Semantic,
            false,
            "deps: [[infra:runbooks/missing.md]] and [[ghost-project:nope.md]]",
        )
        .await;

        let report = run_report(&fx).await;

        let dangling = findings_of_kind(&report, "dangling_cross_project_link");
        assert_eq!(
            dangling.len(),
            2,
            "both unresolved cross-project links reported: {:?}",
            report.findings
        );
        for finding in &dangling {
            assert_eq!(finding.severity, "warning");
            assert_eq!(finding.pages, ["notes/links.md"]);
            assert!(finding.message.contains("notes/links.md"));
        }
        let ghost = dangling
            .iter()
            .find(|f| f.message.contains("default/ghost-project/nope.md"))
            .expect("ghost-project finding");
        assert_eq!(
            ghost.detail.as_ref().unwrap()["project_exists"],
            serde_json::json!(false)
        );
        let infra = dangling
            .iter()
            .find(|f| f.message.contains("default/infra/runbooks/missing.md"))
            .expect("infra finding");
        assert_eq!(
            infra.detail.as_ref().unwrap()["project_exists"],
            serde_json::json!(true)
        );
    }

    #[tokio::test]
    async fn empty_project_reports_no_findings() {
        let fx = seed_fixture().await;

        let report = run_report(&fx).await;

        assert!(report.findings.is_empty());
        assert_eq!(report.summary, "No conservative curator findings.");
        assert!(report.dry_run);
        assert_eq!(report.workspace, "default");
        assert_eq!(report.project, "scratch");
        assert!(!report.generated_at.is_empty());
    }

    fn report_with(findings: Vec<CuratorFinding>) -> CuratorReport {
        let summary = if findings.is_empty() {
            "No conservative curator findings.".to_string()
        } else {
            format!("{} conservative curator finding(s).", findings.len())
        };
        CuratorReport {
            workspace: "default".into(),
            project: "scratch".into(),
            generated_at: "2026-07-23T00:00:00Z".into(),
            dry_run: true,
            summary,
            params: CuratorParams::default(),
            findings,
        }
    }

    #[test]
    fn render_markdown_includes_header_summary_and_findings() {
        let report = report_with(vec![
            CuratorFinding {
                kind: "stale_slot".into(),
                severity: "warning".into(),
                message: "Slot _slots/current-focus.md has not changed for 8 days (threshold 7)"
                    .into(),
                pages: vec!["_slots/current-focus.md".into()],
                detail: None,
            },
            CuratorFinding {
                kind: "cold_episodic".into(),
                severity: "info".into(),
                message: "Episodic page sessions/old.md is cold (score 0.018, age 200 days)".into(),
                pages: Vec::new(),
                detail: None,
            },
        ]);

        let md = render_curator_report_markdown(&report);

        assert!(md.starts_with("# Curator Report\n"));
        assert!(md.contains("Report-only"));
        assert!(md.contains("- Workspace: `default`\n"));
        assert!(md.contains("- Project: `scratch`\n"));
        assert!(md.contains("- Generated: `2026-07-23T00:00:00Z`\n"));
        assert!(md.contains("- Summary: 2 conservative curator finding(s).\n"));
        assert!(md.contains("## Findings\n"));
        assert!(md.contains(
            "- **stale_slot** (warning) — Slot _slots/current-focus.md has not changed for 8 days (threshold 7)\n"
        ));
        assert!(md.contains("  - Pages: `_slots/current-focus.md`\n"));
        assert!(md.contains(
            "- **cold_episodic** (info) — Episodic page sessions/old.md is cold (score 0.018, age 200 days)\n"
        ));
        assert_eq!(
            md.matches("  - Pages:").count(),
            1,
            "findings without pages render no Pages line"
        );
    }

    #[test]
    fn render_markdown_empty_report_omits_findings_section() {
        let md = render_curator_report_markdown(&report_with(Vec::new()));

        let expected = "# Curator Report\n\n\
            > Report-only: approving this pending write stores this report page only. \
            It does not edit, delete, merge, rewrite links, or update slots.\n\n\
            - Workspace: `default`\n\
            - Project: `scratch`\n\
            - Generated: `2026-07-23T00:00:00Z`\n\
            - Summary: No conservative curator findings.\n\n\
            No conservative curator findings.\n";
        assert_eq!(md, expected);
        assert!(!md.contains("## Findings"));
    }

    /// Snapshot of every file under the wiki root: relative path, bytes,
    /// mtime. The `.git` dir is included — a read-only report must not
    /// touch it either.
    fn wiki_snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>, std::time::SystemTime)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let meta = std::fs::metadata(&path).unwrap();
                    out.push((
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        std::fs::read(&path).unwrap(),
                        meta.modified().unwrap(),
                    ));
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Row-level store snapshot via the same reader calls the curator makes.
    async fn store_snapshot(fx: &Fixture) -> String {
        let candidates = fx
            .store
            .reader
            .decay_candidates(fx.ws, fx.proj)
            .await
            .unwrap();
        let pages = fx
            .store
            .reader
            .list_pages("default", "scratch")
            .await
            .unwrap();
        let dangling = fx
            .store
            .reader
            .dangling_cross_project_links(fx.ws, fx.proj)
            .await
            .unwrap();
        serde_json::to_string(&serde_json::json!({
            "candidates": candidates,
            "pages": pages,
            "dangling": dangling,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn report_run_leaves_store_and_wiki_untouched() {
        let fx = seed_fixture().await;
        // Seed one finding of every kind so the run exercises all read paths.
        let cold = write(
            &fx,
            "sessions/old.md",
            "Old Session",
            Tier::Episodic,
            false,
            "old body",
        )
        .await;
        let focus = write(
            &fx,
            "_slots/current-focus.md",
            "Current Focus",
            Tier::Semantic,
            false,
            "focus body",
        )
        .await;
        backdate(&fx, cold, 200);
        backdate(&fx, focus, 8);
        write(
            &fx,
            "notes/dup-a.md",
            "Dup Title",
            Tier::Semantic,
            false,
            "a",
        )
        .await;
        write(
            &fx,
            "notes/dup-b.md",
            "dup title",
            Tier::Semantic,
            false,
            "b",
        )
        .await;
        write(
            &fx,
            "notes/links.md",
            "Links",
            Tier::Semantic,
            false,
            "dep: [[ghost-project:nope.md]]",
        )
        .await;

        let wiki_before = wiki_snapshot(fx.wiki.root());
        let store_before = store_snapshot(&fx).await;

        let report = run_report(&fx).await;
        assert!(
            !report.findings.is_empty(),
            "fixture must produce findings so the read paths actually run"
        );

        assert_eq!(
            store_snapshot(&fx).await,
            store_before,
            "report must not mutate store rows"
        );
        assert_eq!(
            wiki_snapshot(fx.wiki.root()),
            wiki_before,
            "report must not touch wiki files"
        );
    }
}
