//! Typed relation edges, end to end (2.0 item 3, docs/okf.md):
//! a page declaring `relations:` frontmatter produces typed rows in the
//! links table through the real wiki write path, and a declared
//! `contradicts` edge surfaces as a rule-based lint finding with no LLM
//! involved.

use ai_memory_consolidate::{LintOptions, run_lint};
use ai_memory_core::{PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::Store;
use ai_memory_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

async fn scope(store: &Store) -> (WorkspaceId, ProjectId) {
    let ws = store.writer.get_or_create_workspace("w").await.unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "p", None)
        .await
        .unwrap();
    (ws, proj)
}

fn req(
    ws: WorkspaceId,
    proj: ProjectId,
    path: &str,
    body: &str,
    fm: serde_json::Value,
) -> WritePageRequest {
    WritePageRequest {
        workspace_id: ws,
        project_id: proj,
        path: PagePath::new(path).unwrap(),
        frontmatter: fm,
        body: body.into(),
        tier: Tier::Semantic,
        pinned: false,
        title: None,
        admission_ctx: None,
        author_id: None,
        actor: ai_memory_core::ActorContext::anonymous(),
    }
}

#[tokio::test]
async fn declared_relations_become_typed_link_rows() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

    wiki.write_page(req(
        ws,
        proj,
        "gotchas/linker.md",
        "the gotcha",
        serde_json::json!({}),
    ))
    .await
    .unwrap();
    wiki.write_page(req(
        ws,
        proj,
        "notes/fix-log.md",
        "how it was fixed, see [[gotchas/linker.md]]",
        serde_json::json!({"relations": {"fixes": ["gotchas/linker.md"]}}),
    ))
    .await
    .unwrap();

    // Raw rows: one typed edge and one plain reference to the same
    // target coexist (link_type joins the key).
    let db = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
    let rows: Vec<(String, i64)> = db
        .prepare(
            "SELECT link_type, to_page_id IS NOT NULL FROM links \
             WHERE to_path = 'gotchas/linker.md' ORDER BY link_type",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        rows,
        vec![("fixes".to_string(), 1), ("references".to_string(), 1)],
        "typed edge and plain reference must both exist, both resolved"
    );
}

#[tokio::test]
async fn a_declared_contradiction_is_a_lint_finding_without_an_llm() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

    wiki.write_page(req(
        ws,
        proj,
        "decisions/0001.md",
        "we deploy on Fridays",
        serde_json::json!({}),
    ))
    .await
    .unwrap();
    wiki.write_page(req(
        ws,
        proj,
        "notes/new-evidence.md",
        "we stopped deploying on Fridays",
        serde_json::json!({"relations": {"contradicts": ["decisions/0001.md"]}}),
    ))
    .await
    .unwrap();

    let report = run_lint(
        &store.reader,
        &wiki,
        None,
        ws,
        proj,
        LintOptions {
            dry_run: true,
            use_llm: false,
            decay_lambda: 0.02,
        },
    )
    .await
    .unwrap();

    let finding = report
        .findings
        .iter()
        .find(|f| f.kind == "contradiction")
        .expect("declared contradicts must produce a lint finding");
    assert!(finding.message.contains("notes/new-evidence.md"));
    assert!(finding.message.contains("decisions/0001.md"));
    assert_eq!(
        finding.pages,
        vec!["notes/new-evidence.md", "decisions/0001.md"]
    );
}

#[tokio::test]
async fn a_contradiction_to_a_deleted_page_reports_the_stale_declaration() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

    wiki.write_page(req(
        ws,
        proj,
        "notes/orphan-claim.md",
        "contradicts something gone",
        serde_json::json!({"relations": {"contradicts": ["decisions/vanished.md"]}}),
    ))
    .await
    .unwrap();

    let report = run_lint(
        &store.reader,
        &wiki,
        None,
        ws,
        proj,
        LintOptions {
            dry_run: true,
            use_llm: false,
            decay_lambda: 0.02,
        },
    )
    .await
    .unwrap();
    let finding = report
        .findings
        .iter()
        .find(|f| f.kind == "contradiction")
        .expect("unresolved contradicts still lints");
    assert!(finding.message.contains("does not resolve"));
}

// ── 2.0.1: lint reports are one superseding page, not a daily pile ──

/// A page old enough (and cold enough) to trigger the rule-based
/// `stale` finding, so a non-dry run has something to report.
fn stale_page_req(ws: WorkspaceId, proj: ProjectId) -> WritePageRequest {
    let mut r = req(
        ws,
        proj,
        "sessions/ancient.md",
        "long-forgotten episodic capture",
        serde_json::json!({}),
    );
    r.tier = Tier::Episodic;
    r
}

async fn backdate_page(tmp: &TempDir, path: &str, days: i64) {
    let db = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
    let cutoff = jiff::Timestamp::now().as_microsecond() - days * 86_400 * 1_000_000;
    db.execute(
        "UPDATE pages SET updated_at = ?1, created_at = ?1 WHERE path = ?2",
        rusqlite::params![cutoff, path],
    )
    .unwrap();
}

#[tokio::test]
async fn lint_supersedes_one_report_and_prunes_the_legacy_daily_pile() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

    // The pre-2.0.1 accumulation: dated daily reports.
    for date in ["2026-07-01", "2026-07-02", "2026-08-15"] {
        wiki.write_page(req(
            ws,
            proj,
            &format!("_lint/{date}.md"),
            "1 finding(s).",
            serde_json::json!({"kind": "lint-report"}),
        ))
        .await
        .unwrap();
    }
    // Something for the current pass to find.
    wiki.write_page(stale_page_req(ws, proj)).await.unwrap();
    backdate_page(&tmp, "sessions/ancient.md", 90).await;

    let opts = LintOptions {
        dry_run: false,
        use_llm: false,
        decay_lambda: 0.02,
    };
    let report = run_lint(&store.reader, &wiki, None, ws, proj, opts)
        .await
        .unwrap();
    assert!(!report.findings.is_empty(), "the stale page must be found");

    let db = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
    let lint_paths: Vec<String> = db
        .prepare("SELECT path FROM pages WHERE is_latest = 1 AND path LIKE '\\_lint/%' ESCAPE '\\' ORDER BY path")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        lint_paths,
        vec!["_lint/report.md".to_string()],
        "one superseding report; every legacy dated page pruned"
    );

    // A second run with findings still present supersedes in place —
    // still exactly one latest lint page.
    run_lint(&store.reader, &wiki, None, ws, proj, opts)
        .await
        .unwrap();
    let latest_count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE is_latest = 1 AND path LIKE '\\_lint/%' ESCAPE '\\'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(latest_count, 1, "reruns supersede, never accumulate");
}

#[tokio::test]
async fn a_clean_pass_removes_the_stale_report() {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let (ws, proj) = scope(&store).await;
    let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

    // Findings exist -> a report is written.
    wiki.write_page(stale_page_req(ws, proj)).await.unwrap();
    backdate_page(&tmp, "sessions/ancient.md", 90).await;
    let opts = LintOptions {
        dry_run: false,
        use_llm: false,
        decay_lambda: 0.02,
    };
    run_lint(&store.reader, &wiki, None, ws, proj, opts)
        .await
        .unwrap();

    // The offending page goes away; the next clean pass must take the
    // now-false report with it.
    wiki.delete_page(
        ws,
        proj,
        &PagePath::new("sessions/ancient.md").unwrap(),
        None,
        None,
    )
    .await
    .unwrap();
    let report = run_lint(&store.reader, &wiki, None, ws, proj, opts)
        .await
        .unwrap();
    assert!(report.findings.is_empty(), "nothing left to find");

    let db = rusqlite::Connection::open(tmp.path().join("db").join("memory.sqlite")).unwrap();
    let latest_count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM pages WHERE is_latest = 1 AND path LIKE '\\_lint/%' ESCAPE '\\'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(latest_count, 0, "a clean project carries no lint page");
}
