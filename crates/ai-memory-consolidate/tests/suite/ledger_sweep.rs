//! The opt-in event ledger sweep, end to end through a real Store + Wiki.
//!
//! Hook event ledgers (`log-YYYY-MM.md`) are files nothing reads back, and a
//! busy project's month can hold hundreds of megabytes. The sweep deletes the
//! old ones when `[decay] ledger_retention_months` is positive. These tests
//! hold the pass to what makes that safe:
//!
//! 1. **Off by default.** Nothing is deleted at `LedgerRetention::default()`.
//! 2. **A dry run lists and keeps.** Files and bytes are reported, nothing goes.
//! 3. **Age, not name alone.** An old ledger goes; a recent one stays; the
//!    current and the previous month stay even with a one-month retention.
//! 4. **Content, not name alone.** An ordinary page named `log-YYYY-MM.md` is
//!    never deleted, and neither is the unrotated `log.md`.
//! 5. **Scope.** Another project's ledgers are untouched.
//! 6. **Files, not pages.** A legacy `pages` row for a deleted ledger is left
//!    alone and counted in the report.
//! 7. **Admission.** The delete consults the blocking admission chain the way
//!    a page delete does: a `reject`-policy webhook keeps the file and the
//!    report counts it; an accepting chain removes the file and tells the
//!    non-blocking observers afterwards.
//!
//! Fixture names and the pass age against one fixed UTC instant (mid-month,
//! away from a boundary), so a run can never cross the real month boundary
//! between the fixture and the sweep.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use ai_memory_consolidate::sweep::run_sweep_at;
use ai_memory_consolidate::{
    ColdClusterDedup, LedgerRetention, ObservationRetention, SweepError, SweepReport,
    run_sweep_with_ledgers,
};
use ai_memory_core::{NewPage, PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_store::{DecayParams, Store};
use ai_memory_wiki::{AdmissionChain, AdmissionOp, FailurePolicy, WebhookConfig, Wiki};
use jiff::Timestamp;
use tempfile::TempDir;

/// What a hook ledger opens with: `## [ts] <event> | <title>`.
const LEDGER_BODY: &str = "## [2025-01-03T10:00:00Z] user-prompt | hello\n\
                           ## [2025-01-03T10:00:05Z] stop | done\n";
/// An ordinary page that merely shares the ledger's file name.
const PAGE_BODY: &str =
    "# Meeting notes\n\nNot a hook ledger, just a page someone named log-*.md.\n";

/// One fixed instant, mid-UTC-month: the fixture names and the pass age
/// against this same clock, so no run can straddle the real month boundary.
/// The epoch seconds are 2026-07-15T12:00:00Z.
fn now() -> Timestamp {
    Timestamp::new(1_784_116_800, 0).unwrap()
}

struct Fixture {
    _tmp: TempDir,
    store: Store,
    wiki: Wiki,
    ws: WorkspaceId,
    proj: ProjectId,
    other: ProjectId,
}

async fn fixture_with_chain(chain: Option<AdmissionChain>) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let store = Store::open(tmp.path()).unwrap();
    let ws = store
        .writer
        .get_or_create_workspace("default")
        .await
        .unwrap();
    let proj = store
        .writer
        .get_or_create_project(ws, "ledgers", None)
        .await
        .unwrap();
    let other = store
        .writer
        .get_or_create_project(ws, "neighbour", None)
        .await
        .unwrap();
    let mut wiki = Wiki::new(tmp.path(), store.writer.clone())
        .unwrap()
        .with_store_reader(store.reader.clone());
    if let Some(chain) = chain {
        wiki = wiki.with_admission_chain(chain);
    }
    for project in [proj, other] {
        std::fs::create_dir_all(wiki.project_root(ws, project)).unwrap();
    }
    Fixture {
        _tmp: tmp,
        store,
        wiki,
        ws,
        proj,
        other,
    }
}

async fn fixture() -> Fixture {
    fixture_with_chain(None).await
}

/// `log-YYYY-MM.md` for the month `back` months before the fixture's month.
fn month_file(back: i64) -> String {
    let now_utc = now().to_zoned(jiff::tz::TimeZone::UTC);
    let index = i64::from(now_utc.year()) * 12 + i64::from(now_utc.month()) - 1 - back;
    format!(
        "log-{:04}-{:02}.md",
        index.div_euclid(12),
        index.rem_euclid(12) + 1
    )
}

fn put(fx: &Fixture, project: ProjectId, name: &str, body: &str) -> std::path::PathBuf {
    let path = fx.wiki.project_root(fx.ws, project).join(name);
    std::fs::write(&path, body).unwrap();
    path
}

async fn sweep(fx: &Fixture, project: ProjectId, months: i64, dry_run: bool) -> SweepReport {
    run_sweep_at(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        project,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        false,
        ColdClusterDedup::default(),
        LedgerRetention { months },
        now(),
        dry_run,
    )
    .await
    .unwrap()
}

/// Minimal loopback webhook server for the admission tests: answers every
/// POST with one fixed status and records the `X-Memory-Op` header of each
/// request. Built on `std::net` — this harness has no HTTP server crate as a
/// dev-dependency — and it speaks just enough HTTP/1.1 for the engine's
/// `reqwest` client: one `Content-Length`-framed body per call, answered with
/// `Connection: close` so the client opens a fresh connection per call. The
/// listener thread lives for the rest of the process, as the test-only
/// `dead_http_endpoint` helper does.
fn webhook(status: u16, ops: &Arc<Mutex<Vec<String>>>) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ops = Arc::clone(ops);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read the head until the blank line that ends the headers.
            let head_end = loop {
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(pos + 4);
                }
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            };
            let Some(end) = head_end else { continue };
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let op = head
                .lines()
                .find_map(|l| l.strip_prefix("x-memory-op:").map(str::to_owned))
                .map(|v| v.trim().to_string());
            let body_len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            // Drain the body before answering: the client may still be
            // writing it, and a premature close surfaces as a spurious
            // connection error.
            while buf.len() < end + body_len {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            if let Some(op) = op {
                ops.lock().unwrap().push(op);
            }
            let resp =
                format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn the_default_configuration_deletes_no_ledger() {
    let fx = fixture().await;
    let old = put(&fx, fx.proj, &month_file(36), LEDGER_BODY);

    let report = sweep(&fx, fx.proj, 0, false).await;

    assert!(old.exists(), "retention 0 must keep every ledger");
    assert_eq!(report.ledgers_prunable, 0);
    assert_eq!(report.ledgers_pruned, 0);
    assert_eq!(report.ledger_bytes_prunable, 0);
    assert_eq!(report.ledger_bytes_pruned, 0);
}

#[tokio::test]
async fn a_dry_run_reports_files_and_bytes_and_deletes_nothing() {
    let fx = fixture().await;
    let old_a = put(&fx, fx.proj, &month_file(24), LEDGER_BODY);
    let old_b = put(&fx, fx.proj, &month_file(10), LEDGER_BODY);
    let recent = put(&fx, fx.proj, &month_file(1), LEDGER_BODY);

    let report = sweep(&fx, fx.proj, 3, true).await;

    assert!(report.dry_run);
    assert_eq!(report.ledgers_prunable, 2);
    assert_eq!(report.ledger_bytes_prunable, 2 * LEDGER_BODY.len() as u64);
    assert_eq!(report.ledgers_pruned, 0, "a dry run deletes nothing");
    assert_eq!(report.ledger_bytes_pruned, 0);
    assert!(old_a.exists() && old_b.exists() && recent.exists());
}

#[tokio::test]
async fn an_old_ledger_is_deleted_and_a_recent_one_is_kept() {
    let fx = fixture().await;
    let old = put(&fx, fx.proj, &month_file(24), LEDGER_BODY);
    let four_back = put(&fx, fx.proj, &month_file(4), LEDGER_BODY);
    // Three whole months have not yet passed after this month ended.
    let three_back = put(&fx, fx.proj, &month_file(3), LEDGER_BODY);
    let recent = put(&fx, fx.proj, &month_file(0), LEDGER_BODY);

    let report = sweep(&fx, fx.proj, 3, false).await;

    assert!(!old.exists(), "a two-year-old ledger goes");
    assert!(
        !four_back.exists(),
        "the first month past the threshold goes"
    );
    assert!(
        three_back.exists(),
        "one month short of the threshold stays"
    );
    assert!(recent.exists(), "the current month stays");
    assert_eq!(report.ledgers_prunable, 2);
    assert_eq!(report.ledgers_pruned, 2);
    assert_eq!(report.ledger_bytes_pruned, 2 * LEDGER_BODY.len() as u64);
    assert!(!report.dry_run);
}

#[tokio::test]
async fn the_current_and_previous_month_survive_even_a_one_month_retention() {
    let fx = fixture().await;
    let current = put(&fx, fx.proj, &month_file(0), LEDGER_BODY);
    let previous = put(&fx, fx.proj, &month_file(1), LEDGER_BODY);
    // Control: the month before that is old enough at N = 1 and must go, so the
    // two survivors above are protected by the guard and not by an empty pass.
    let two_back = put(&fx, fx.proj, &month_file(2), LEDGER_BODY);

    let report = sweep(&fx, fx.proj, 1, false).await;

    assert!(
        current.exists(),
        "the writer may still append to this month"
    );
    assert!(previous.exists(), "late events still land in last month");
    assert!(!two_back.exists(), "control: a closed month is deletable");
    assert_eq!(report.ledgers_pruned, 1);
}

#[tokio::test]
async fn an_ordinary_page_named_like_a_ledger_is_never_deleted() {
    let fx = fixture().await;
    let page = put(&fx, fx.proj, &month_file(36), PAGE_BODY);
    // Frontmatter does not make a page a ledger either (a migrated store stamps
    // frontmatter on real ledgers, so the gate looks past it to the body).
    let framed = put(
        &fx,
        fx.proj,
        &month_file(35),
        "---\ntitle: Quarterly log\n---\n\n# Quarterly log\n\nplain text\n",
    );
    // The unrotated ledger is never swept, whatever its content.
    let unrotated = put(&fx, fx.proj, "log.md", LEDGER_BODY);
    // Control: a real old ledger in the same project is deleted by the same run.
    let ledger = put(&fx, fx.proj, &month_file(34), LEDGER_BODY);

    let report = sweep(&fx, fx.proj, 3, false).await;

    assert!(page.exists(), "a page that only shares the name must stay");
    assert_eq!(std::fs::read_to_string(&page).unwrap(), PAGE_BODY);
    assert!(
        framed.exists(),
        "frontmatter without a ledger entry must stay"
    );
    assert!(unrotated.exists(), "log.md is never swept");
    assert!(!ledger.exists(), "control: the real ledger goes");
    assert_eq!(report.ledgers_pruned, 1);
    // The report must not claim files it will not delete: the pages are not
    // even matched.
    assert_eq!(report.ledgers_prunable, 1);
    assert_eq!(report.ledger_bytes_prunable, LEDGER_BODY.len() as u64);
}

#[tokio::test]
async fn another_projects_ledgers_are_untouched() {
    let fx = fixture().await;
    let mine = put(&fx, fx.proj, &month_file(24), LEDGER_BODY);
    let theirs = put(&fx, fx.other, &month_file(24), LEDGER_BODY);

    let report = sweep(&fx, fx.proj, 3, false).await;

    assert!(!mine.exists(), "the swept project's ledger goes");
    assert!(theirs.exists(), "a neighbouring project's ledger stays");
    assert_eq!(report.ledgers_pruned, 1);

    // And the neighbour's own sweep is what reaches its file.
    let report = sweep(&fx, fx.other, 3, false).await;
    assert!(!theirs.exists());
    assert_eq!(report.ledgers_pruned, 1);
}

#[tokio::test]
async fn a_legacy_page_row_is_left_alone_and_reported() {
    let fx = fixture().await;
    let name = month_file(24);
    put(&fx, fx.proj, &name, LEDGER_BODY);
    fx.store
        .writer
        .upsert_page(NewPage {
            workspace_id: fx.ws,
            project_id: fx.proj,
            path: PagePath::new(name.clone()).unwrap(),
            title: name.clone(),
            body: LEDGER_BODY.into(),
            tier: Tier::Episodic,
            frontmatter_json: serde_json::json!({}),
            pinned: false,
            links: Vec::new(),
            author_id: None,
            expires_at: None,
            entities: Vec::new(),
            evidence: Vec::new(),
        })
        .await
        .unwrap();

    let report = sweep(&fx, fx.proj, 3, false).await;

    assert_eq!(report.ledgers_pruned, 1, "the file goes");
    assert_eq!(
        report.ledger_page_rows_left, 1,
        "the legacy row is reported"
    );
    assert!(
        fx.store
            .reader
            .latest_page_id_by_ids(fx.ws, fx.proj, name)
            .await
            .unwrap()
            .is_some(),
        "the sweep never touches the store for a ledger"
    );
}

#[tokio::test]
async fn a_negative_ledger_retention_stops_the_sweep() {
    let fx = fixture().await;
    let old = put(&fx, fx.proj, &month_file(24), LEDGER_BODY);

    let error = run_sweep_with_ledgers(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        false,
        ColdClusterDedup::default(),
        LedgerRetention { months: -1 },
        false,
    )
    .await
    .expect_err("a negative age is not a cutoff");

    assert!(matches!(error, SweepError::InvalidLedgerRetention));
    assert!(old.exists());
}

#[tokio::test]
async fn without_a_wiki_handle_the_pass_is_skipped() {
    let fx = fixture().await;
    let old = put(&fx, fx.proj, &month_file(24), LEDGER_BODY);

    let report = run_sweep_with_ledgers(
        &fx.store.reader,
        &fx.store.writer,
        None,
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        false,
        ColdClusterDedup::default(),
        LedgerRetention { months: 3 },
        false,
    )
    .await
    .unwrap();

    assert!(
        old.exists(),
        "a bare-store caller has no wiki to delete through"
    );
    assert_eq!(report.ledgers_pruned, 0);
}

#[tokio::test]
async fn a_rejecting_admission_webhook_keeps_the_ledger_and_the_report_says_so() {
    let ops = Arc::new(Mutex::new(Vec::new()));
    let base = webhook(403, &ops);
    let chain = AdmissionChain::new(vec![WebhookConfig {
        name: "refuse-ledger-deletes".into(),
        url: format!("{base}/hook"),
        timeout_ms: 2_000,
        failure_policy: FailurePolicy::Reject,
        events: vec![AdmissionOp::DeleteEventLedger],
        blocking: true,
        projects: Vec::new(),
        headers: Vec::new(),
    }])
    .unwrap();
    let fx = fixture_with_chain(Some(chain)).await;
    let old = put(&fx, fx.proj, &month_file(24), LEDGER_BODY);
    let recent = put(&fx, fx.proj, &month_file(0), LEDGER_BODY);

    let report = run_sweep_at(
        &fx.store.reader,
        &fx.store.writer,
        Some(&fx.wiki),
        fx.ws,
        fx.proj,
        &DecayParams::default(),
        0.0,
        ObservationRetention::default(),
        false,
        ColdClusterDedup::default(),
        LedgerRetention { months: 3 },
        now(),
        false,
    )
    .await
    .unwrap();

    assert!(
        old.exists(),
        "a refusing webhook keeps the file, like a refused page delete"
    );
    assert_eq!(
        std::fs::read_to_string(&old).unwrap(),
        LEDGER_BODY,
        "the file is not just present, it is intact"
    );
    assert!(recent.exists());
    assert_eq!(report.ledgers_prunable, 1);
    assert_eq!(report.ledgers_pruned, 0, "nothing went");
    assert_eq!(
        report.ledgers_undeleted, 1,
        "the report counts the refusal, so it is visible without reading logs"
    );
    assert_eq!(
        *ops.lock().unwrap(),
        vec!["delete_event_ledger".to_string()],
        "the sweep asked the ledger op, not a page delete"
    );
}

#[tokio::test]
async fn an_accepting_admission_webhook_is_told_after_the_file_is_gone() {
    let decider_ops = Arc::new(Mutex::new(Vec::new()));
    let observer_ops = Arc::new(Mutex::new(Vec::new()));
    let decider = webhook(204, &decider_ops);
    let observer = webhook(204, &observer_ops);
    let chain = AdmissionChain::new(vec![
        WebhookConfig {
            name: "ledger-gate".into(),
            url: format!("{decider}/gate"),
            timeout_ms: 2_000,
            failure_policy: FailurePolicy::Reject,
            events: vec![AdmissionOp::DeleteEventLedger],
            blocking: true,
            projects: Vec::new(),
            headers: Vec::new(),
        },
        WebhookConfig {
            name: "ledger-mirror".into(),
            url: format!("{observer}/mirror"),
            timeout_ms: 2_000,
            failure_policy: FailurePolicy::Ignore,
            events: vec![AdmissionOp::DeleteEventLedger],
            blocking: false,
            projects: Vec::new(),
            headers: Vec::new(),
        },
    ])
    .unwrap();
    let fx = fixture_with_chain(Some(chain)).await;
    let old = put(&fx, fx.proj, &month_file(24), LEDGER_BODY);

    let report = sweep(&fx, fx.proj, 3, false).await;

    assert!(!old.exists(), "the accepting chain lets the delete through");
    assert_eq!(report.ledgers_pruned, 1);
    assert_eq!(report.ledgers_undeleted, 0);
    assert_eq!(
        *decider_ops.lock().unwrap(),
        vec!["delete_event_ledger".to_string()],
        "the blocking gate was consulted before the removal"
    );
    // The observer is dispatched fire-and-forget after the removal, like a
    // page delete tells its mirrors; poll for it instead of racing it.
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if observer_ops
                .lock()
                .unwrap()
                .iter()
                .any(|op| op == "delete_event_ledger")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the observer was told after the file was gone");
}
