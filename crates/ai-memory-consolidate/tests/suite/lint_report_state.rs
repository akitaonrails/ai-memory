//! #2021: what `_lint/report.md` records for the next run and for readers.
//!
//! 1. The frontmatter carries the finding counts and the first warning pages,
//!    which `memory_briefing` and the session-start notice read back.
//! 2. A rule-only run asked to keep LLM findings copies the previous LLM
//!    pass into its report, so the daily rule lint does not erase a weekly
//!    scheduled contradiction pass. Without the flag it drops them, as
//!    before.
//! 3. An LLM pass that fails keeps the previous LLM findings when asked.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use ai_memory_consolidate::{LintOptions, run_lint};
use ai_memory_core::{ActorContext, OwnerFilter, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{ChatRequest, ChatResponse, LlmError, LlmProvider, LlmResult};
use ai_memory_store::Store;
use ai_memory_wiki::{Wiki, WritePageRequest};
use tempfile::TempDir;

struct Fixture {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
}

async fn fixture() -> Fixture {
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
    let wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    Fixture {
        _tmp: tmp,
        store,
        wiki,
        ws,
        proj,
    }
}

async fn write_page(fx: &Fixture, path: &str, frontmatter: serde_json::Value, body: &str) {
    fx.wiki
        .write_page(WritePageRequest {
            workspace_id: fx.ws,
            project_id: fx.proj,
            path: PagePath::new(path).unwrap(),
            frontmatter,
            body: body.to_string(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: ActorContext::anonymous(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();
}

/// Answers each structured call with the next scripted reply.
struct Scripted(Mutex<VecDeque<Option<serde_json::Value>>>);

impl Scripted {
    fn provider(
        replies: impl IntoIterator<Item = Option<serde_json::Value>>,
    ) -> Arc<dyn LlmProvider> {
        Arc::new(Self(Mutex::new(replies.into_iter().collect())))
    }
}

impl LlmProvider for Scripted {
    fn name(&self) -> &'static str {
        "scripted"
    }
    fn model(&self) -> &str {
        "scripted"
    }
    fn complete<'life0, 'async_trait>(
        &'life0 self,
        _request: ChatRequest,
    ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { unreachable!("the lint pass asks for structured output") })
    }
    fn complete_structured_raw<'life0, 'async_trait>(
        &'life0 self,
        _request: ChatRequest,
        _schema: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        let reply = self.0.lock().unwrap().pop_front().expect("unscripted call");
        Box::pin(async move {
            reply.ok_or_else(|| LlmError::UnexpectedShape("scripted failure".into()))
        })
    }
}

fn contradiction(message: &str) -> serde_json::Value {
    serde_json::json!({ "findings": [{
        "kind": "contradiction",
        "severity": "warning",
        "message": message,
        "pages": ["concepts/a.md", "concepts/b.md"],
    }]})
}

fn options(use_llm: bool, keep_llm_findings: bool) -> LintOptions {
    LintOptions {
        dry_run: false,
        use_llm,
        decay: ai_memory_store::DecayParams::default(),
        embedding: None,
        contradiction_band_min: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_LOW,
        contradiction_band_max: ai_memory_consolidate::DEFAULT_CONTRADICTION_SIM_HIGH,
        keep_llm_findings,
    }
}

async fn lint(
    fx: &Fixture,
    llm: Option<&Arc<dyn LlmProvider>>,
    options: LintOptions,
) -> Vec<String> {
    run_lint(&fx.store.reader, &fx.wiki, llm, fx.ws, fx.proj, options)
        .await
        .unwrap()
        .findings
        .into_iter()
        .map(|f| f.message)
        .collect()
}

fn report_body(fx: &Fixture) -> String {
    fx.wiki
        .read_page(fx.ws, fx.proj, &PagePath::new("_lint/report.md").unwrap())
        .unwrap()
        .body
}

async fn seed_knowledge(fx: &Fixture) {
    write_page(
        fx,
        "concepts/a.md",
        serde_json::json!({"title": "A"}),
        "Deploys run on Fridays.",
    )
    .await;
    write_page(
        fx,
        "concepts/b.md",
        serde_json::json!({"title": "B"}),
        "Deploys never run on Fridays.",
    )
    .await;
}

#[tokio::test]
async fn a_rule_only_run_keeps_llm_findings_only_when_asked() {
    let fx = fixture().await;
    seed_knowledge(&fx).await;
    let llm = Scripted::provider([Some(contradiction("Friday deploys disagree"))]);

    let first = lint(&fx, Some(&llm), options(true, false)).await;
    assert_eq!(first, ["Friday deploys disagree"]);
    assert!(report_body(&fx).contains("LLM contradiction pass:"));

    // The scheduled rule pass while the LLM pass is on: no provider call
    // (the script is empty), the LLM finding is copied forward.
    let kept = lint(&fx, Some(&llm), options(false, true)).await;
    assert_eq!(kept, ["Friday deploys disagree"]);
    assert!(report_body(&fx).contains("Friday deploys disagree"));
    // And again from the copied report, not only from the original one.
    let kept = lint(&fx, None, options(false, true)).await;
    assert_eq!(kept, ["Friday deploys disagree"]);

    // Control: without the flag a rule-only run reports rule findings only.
    let dropped = lint(&fx, None, options(false, false)).await;
    assert!(dropped.is_empty(), "{dropped:?}");
}

#[tokio::test]
async fn a_failed_llm_pass_keeps_the_previous_llm_findings() {
    let fx = fixture().await;
    seed_knowledge(&fx).await;
    let llm = Scripted::provider([
        Some(contradiction("first pass")),
        None,
        Some(serde_json::json!({ "findings": [] })),
    ]);

    assert_eq!(
        lint(&fx, Some(&llm), options(true, true)).await,
        ["first pass"]
    );
    assert_eq!(
        lint(&fx, Some(&llm), options(true, true)).await,
        ["first pass"]
    );
    // A pass that runs and finds nothing replaces the old findings.
    assert!(lint(&fx, Some(&llm), options(true, true)).await.is_empty());
}

#[tokio::test]
async fn the_report_frontmatter_feeds_the_lint_summary_and_briefing() {
    let fx = fixture().await;
    let reader = &fx.store.reader;
    assert_eq!(reader.lint_summary(fx.ws, fx.proj).await.unwrap(), None);

    // Seven pairs sharing a title: seven `duplicate` warnings naming
    // fourteen pages, of which the summary lists the first five.
    for i in 0..7 {
        for side in ["a", "b"] {
            write_page(
                &fx,
                &format!("notes/{i}-{side}.md"),
                serde_json::json!({"title": format!("Shared {i}")}),
                "body",
            )
            .await;
        }
    }
    write_page(
        &fx,
        "_rules/style.md",
        serde_json::json!({"title": "Style"}),
        "Use tabs.",
    )
    .await;
    lint(&fx, None, options(false, false)).await;

    let summary = reader.lint_summary(fx.ws, fx.proj).await.unwrap().unwrap();
    assert_eq!(summary.report_path, "_lint/report.md");
    assert_eq!(summary.warnings, 7);
    // The `_rules/` page adds an `info` rule suggestion.
    assert_eq!(summary.findings, 8);
    assert_eq!(summary.warning_pages.len(), 5);
    assert!(
        summary
            .warning_pages
            .iter()
            .all(|p| p.starts_with("notes/"))
    );

    let briefing = reader
        .briefing_for_project_with_slot_visibility(
            fx.ws,
            fx.proj,
            10,
            OwnerFilter::Any,
            &ai_memory_core::SlotVisibility::All,
            false,
        )
        .await
        .unwrap();
    assert_eq!(briefing.lint, Some(summary));
}

/// The report is an ordinary page. A report written before #2021 has no
/// counts and yields no summary; a hand-edited list is still bounded.
#[tokio::test]
async fn a_report_without_counts_or_with_a_long_list_stays_bounded() {
    let fx = fixture().await;
    let reader = &fx.store.reader;
    let path = "_lint/report.md";
    write_page(
        &fx,
        path,
        serde_json::json!({"title": "Lint report", "kind": "lint-report"}),
        "# old",
    )
    .await;
    assert_eq!(reader.lint_summary(fx.ws, fx.proj).await.unwrap(), None);

    let pages: Vec<String> = (0..40).map(|i| format!("notes/{i}.md")).collect();
    write_page(
        &fx,
        path,
        serde_json::json!({"title": "Lint report", "warnings": 40, "findings": 40, "warning_pages": pages}),
        "# edited",
    )
    .await;
    let summary = reader.lint_summary(fx.ws, fx.proj).await.unwrap().unwrap();
    assert_eq!(summary.warnings, 40);
    assert_eq!(summary.warning_pages, pages[..5]);
}
