//! `ai-memory purge-session` — thin HTTP client for single-session purge.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::PurgeSessionArgs;
use crate::commands::purge_preview::{PreviewOutcome, refused_message, run_preview};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/purge-session`.
#[derive(Serialize)]
struct PurgeSessionRequest {
    workspace: String,
    project: String,
    session_id: String,
    confirm: bool,
    /// Rebuild the FTS indexes and VACUUM after the delete commits.
    compact: bool,
    /// Preview only: wins over `confirm` on the server (mirrors
    /// `purge-project`, which itself mirrors `reclaim-ledger-versions`), so
    /// this is always sent alongside `confirm: false` here — never both
    /// true. Older servers that predate this field simply never look at it
    /// (an unknown JSON field is not a deserialize error), which is why the
    /// fallback in [`purge_preview::run_preview`](crate::commands::purge_preview::run_preview)
    /// only has to handle the request failing outright, never the field
    /// being silently misread.
    dry_run: bool,
}

/// One line naming what a session purge (real or previewed) removed, in the
/// fixed order the operator can grep for: observations, handoffs, pages,
/// auto-improve runs. `verb` is `"Purged"` for a confirmed run or `"Would
/// purge"` for a preview — everything else about the line is identical, so a
/// script matching one also matches the other.
///
/// The session id is deliberately never part of this line, confirmed or
/// previewed: the caller already has it (they passed `--session-id`), and
/// this command exists to make a session stop existing — echoing its id
/// into terminal scrollback and shell history leaves a pointer to the thing
/// just erased (or about to be). The scope (`label`) and the counts are what
/// confirm the operation did, or would do, what was asked.
fn purge_summary_line(verb: &str, label: &str, report: &serde_json::Value) -> String {
    let observations = report["observations_deleted"].as_u64().unwrap_or(0);
    let handoffs = report["handoffs_deleted"].as_u64().unwrap_or(0);
    let pages = report["pages_deleted"].as_u64().unwrap_or(0);
    let auto_improve_runs = report["auto_improve_runs_deleted"].as_u64().unwrap_or(0);
    let mut line = format!(
        "{verb} session from {label}: {observations} observations, {handoffs} handoffs, \
         {pages} pages, {auto_improve_runs} auto-improve runs."
    );
    // The mirror of the incident `purge-project`'s own preview guards
    // against, one level down at session granularity: purging this session
    // also collaterally deletes/orphans rows that live in *other* projects,
    // via this session's own id (not the project's rows) cascading. Silent
    // when zero so the common case reads exactly as before.
    let collateral_observations = report["collateral_observations_deleted"]
        .as_u64()
        .unwrap_or(0);
    if collateral_observations > 0 {
        line.push_str(&format!(
            " Plus {collateral_observations} observations in other projects via this session."
        ));
    }
    let collateral_handoffs = report["collateral_handoffs_denulled"].as_u64().unwrap_or(0);
    if collateral_handoffs > 0 {
        line.push_str(&format!(
            " Plus {collateral_handoffs} handoffs in other projects that will lose their \
             session reference (set to NULL, not deleted)."
        ));
    }
    line
}

/// Decide what to print, if anything, before the refusal — pure, so it is
/// unit-tested without a server. `fallback_label` is used only when the
/// server's own report has no scope to build a label from (this endpoint has
/// no `label` field, unlike `purge-project`, so the caller always supplies
/// one built from `--workspace`/`--project`).
fn preview_message(outcome: &PreviewOutcome, label: &str) -> Option<String> {
    match outcome {
        PreviewOutcome::Previewed(report) => Some(purge_summary_line("Would purge", label, report)),
        PreviewOutcome::Refused { status, body } => Some(refused_message(*status, body)),
        PreviewOutcome::Ignored | PreviewOutcome::OlderServer | PreviewOutcome::Unreachable => None,
    }
}

/// Run the `purge-session` subcommand.
///
/// Requires the full session UUID and `--confirm`. The scope is resolved the
/// same way as every other project-scoped command, and the server refuses a
/// session that does not belong to it, so a UUID alone is never authority
/// over another workspace or project.
///
/// Without `--confirm`, first asks the server for a preview (`dry_run:
/// true`, which wins over `confirm` server-side, exactly like
/// `purge-project`): the server reports the counts a confirmed purge would
/// produce without deleting anything. That preview is best-effort, bounded
/// by
/// [`purge_preview::PREVIEW_TIMEOUT`](crate::commands::purge_preview::PREVIEW_TIMEOUT),
/// and never changes the outcome — only what gets printed before it:
/// - a successful preview prints the "Would purge session from ..." line;
/// - a 404/403 (or any other unexpected status) prints the server's own
///   error first, since the operator asked what would happen and the server
///   has an answer, just not the one this command expected;
/// - a plain 400 (an older server that predates `dry_run`), a timeout, or an
///   unreachable server print nothing extra — the refusal below already
///   says everything a 400 would.
///
/// # Errors
/// Returns an error when `--confirm` is absent, the session id is not a
/// UUID, the server is unreachable, or the server returns a non-2xx
/// response (including `404` for a session outside the named scope).
pub async fn run(config: &Config, args: PurgeSessionArgs) -> Result<()> {
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.project.as_deref(), "--project")?;
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;

    // Validate locally so an obvious typo fails before anything destructive
    // is sent. The server validates again — this is convenience, not the
    // security boundary.
    let session_id = args.session_id.trim();
    if uuid::Uuid::parse_str(session_id).is_err() {
        bail!(
            "{}", i18n.format("cli-purge-session-session-id-session-id-is-not-a-uuid-pass-the-full-sessions-id-value-a-prefix", &[("value0", session_id.to_string())]));
    }

    let label = format!("{workspace}/{project}");

    if !args.confirm {
        let request = PurgeSessionRequest {
            workspace: workspace.clone(),
            project: project.clone(),
            session_id: session_id.to_owned(),
            confirm: false,
            compact: args.compact,
            dry_run: true,
        };
        let outcome = run_preview(config, "/admin/purge-session", &request).await;
        if let Some(line) = preview_message(&outcome, &label) {
            println!("{line}");
        }
        bail!(
            "{}", i18n.format("cli-purge-session-purge-session-is-destructive-and-irreversible-re-run-with-confirm-to-proceed", &[("value0", workspace.to_string()), ("value1", project.to_string()), ("value2", session_id.to_string())]));
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/purge-session",
        &PurgeSessionRequest {
            workspace: workspace.clone(),
            project: project.clone(),
            session_id: session_id.to_owned(),
            confirm: true,
            compact: args.compact,
            dry_run: false,
        },
    )
    .await?;

    println!("{}", purge_summary_line("Purged", &label, &report));
    if let Some(paths) = report["files_deleted"].as_array()
        && !paths.is_empty()
    {
        println!("{}", i18n.text("cli-purge-session-wiki-pages-removed"));
        for path in paths.iter().filter_map(serde_json::Value::as_str) {
            println!("  - {path}");
        }
    }
    if let Some(failed) = report["files_failed"].as_array()
        && !failed.is_empty()
    {
        println!(
            "{}", i18n.format("cli-purge-session-warning-wiki-page-file-s-could-not-be-removed-from-disk-db-rows-are-gone", &[("value0", format!("{}", failed.len()))]));
        for path in failed.iter().filter_map(serde_json::Value::as_str) {
            println!("  - {path}");
        }
    }
    if report["compacted"].as_bool().unwrap_or(false) {
        println!(
            "{}",
            i18n.text("cli-purge-project-database-compacted-freed-bytes-reclaimed")
        );
    } else {
        println!(
            "{}", i18n.text("cli-purge-session-note-this-was-a-logical-delete-the-session-is-unreachable-through-the-api-an"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(dry_run: bool) -> serde_json::Value {
        serde_json::json!({
            "session_id": "00000000-0000-0000-0000-000000000000",
            "workspace": "default",
            "project": "my-project",
            "observations_deleted": 1063,
            "handoffs_deleted": 0,
            "pages_deleted": 1,
            "auto_improve_runs_deleted": 2,
            "removed_paths": [],
            "collateral_observations_deleted": 0,
            "collateral_handoffs_denulled": 0,
            "files_deleted": [],
            "files_failed": [],
            "compacted": false,
            "dry_run": dry_run,
        })
    }

    /// Pins the exact wording and field order a script would grep for, and
    /// that the session id never appears in it.
    /// `purge_summary_line` is the single source for both the confirmed
    /// "Purged" line and the preview's "Would purge" line, so this also
    /// proves the two can never drift apart.
    #[test]
    fn purge_summary_line_matches_the_documented_wording() {
        let confirmed = report(false);
        let line = purge_summary_line("Purged", "default/my-project", &confirmed);
        assert_eq!(
            line,
            "Purged session from default/my-project: 1063 observations, 0 handoffs, \
             1 pages, 2 auto-improve runs."
        );
        assert!(
            !line.contains("00000000"),
            "the session id must never appear in the printed line: {line}"
        );

        let preview = report(true);
        assert_eq!(
            purge_summary_line("Would purge", "default/my-project", &preview),
            "Would purge session from default/my-project: 1063 observations, 0 handoffs, \
             1 pages, 2 auto-improve runs."
        );
    }

    /// Missing counters must not panic and must not silently show as
    /// non-zero.
    #[test]
    fn purge_summary_line_defaults_missing_counters_to_zero() {
        let empty = serde_json::json!({});
        assert_eq!(
            purge_summary_line("Would purge", "default/x", &empty),
            "Would purge session from default/x: 0 observations, 0 handoffs, \
             0 pages, 0 auto-improve runs."
        );
    }

    /// The mirror-of-the-incident collateral counts, when present, must be
    /// visible in the same line the operator already reads — silent when
    /// zero (the common case), spelled out when not.
    #[test]
    fn purge_summary_line_calls_out_collateral_damage_when_present() {
        let mut r = report(true);
        r["collateral_observations_deleted"] = serde_json::json!(7);
        r["collateral_handoffs_denulled"] = serde_json::json!(2);
        let line = purge_summary_line("Would purge", "default/looks-empty", &r);
        assert!(
            line.contains("Plus 7 observations in other projects via this session."),
            "collateral observations must be called out: {line}"
        );
        assert!(
            line.contains("Plus 2 handoffs in other projects"),
            "collateral handoffs must be called out: {line}"
        );
    }

    /// A successful preview prints the "Would purge" line.
    #[test]
    fn preview_message_prints_the_would_purge_line_on_success() {
        let outcome = PreviewOutcome::Previewed(report(true));
        let msg = preview_message(&outcome, "default/my-project").expect("must print a line");
        assert!(msg.starts_with("Would purge session from default/my-project:"));
    }

    /// A 404 (session outside the named scope) is worth showing before the
    /// refusal: the operator asked what would happen, and 404 is the
    /// server's answer.
    #[test]
    fn preview_message_surfaces_a_404_before_the_refusal() {
        let outcome = PreviewOutcome::Refused {
            status: 404,
            body: r#"{"error":"session ... not found in this workspace/project"}"#.to_string(),
        };
        let msg = preview_message(&outcome, "default/ghost").expect("must print a line");
        assert_eq!(
            msg,
            "Preview refused (404): session ... not found in this workspace/project"
        );
    }

    /// A body that isn't the expected `{"error": ...}` shape still prints
    /// something rather than nothing — the raw body, verbatim.
    #[test]
    fn preview_message_falls_back_to_the_raw_body_when_not_json() {
        let outcome = PreviewOutcome::Refused {
            status: 403,
            body: "Forbidden".to_string(),
        };
        let msg = preview_message(&outcome, "default/x").expect("must print a line");
        assert_eq!(msg, "Preview refused (403): Forbidden");
    }

    /// The three silent-fallback cases: an older server's plain 400, a
    /// timeout/connect failure, and a 200 that oddly never set `dry_run`.
    #[test]
    fn preview_message_is_silent_for_older_server_unreachable_and_ignored() {
        assert!(preview_message(&PreviewOutcome::OlderServer, "default/x").is_none());
        assert!(preview_message(&PreviewOutcome::Unreachable, "default/x").is_none());
        assert!(preview_message(&PreviewOutcome::Ignored, "default/x").is_none());
    }

    // -----------------------------------------------------------------
    // `run_preview` against a real (local) server: proves the HTTP-status
    // classification end to end, not just the pure `preview_message` mapping
    // above.
    // -----------------------------------------------------------------

    fn config_for(tmp: &tempfile::TempDir, server_url: String) -> Config {
        Config {
            data_dir: tmp.path().to_path_buf(),
            server_url,
            ..Config::default()
        }
    }

    async fn spawn_fixed_response(status: u16, body: &'static str) -> String {
        let app = axum::Router::new().route(
            "/admin/purge-session",
            axum::routing::post(move || async move {
                (
                    axum::http::StatusCode::from_u16(status).unwrap(),
                    axum::Json(serde_json::from_str::<serde_json::Value>(body).unwrap()),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn preview_request() -> PurgeSessionRequest {
        PurgeSessionRequest {
            workspace: "default".into(),
            project: "scratch".into(),
            session_id: "00000000-0000-0000-0000-000000000000".into(),
            confirm: false,
            compact: false,
            dry_run: true,
        }
    }

    /// An older server that predates `dry_run` answers the plain 400
    /// "confirm=true" refusal it always has — the CLI must fall back
    /// silently, not print anything extra.
    #[tokio::test]
    async fn run_preview_classifies_a_400_as_older_server() {
        let tmp = tempfile::TempDir::new().unwrap();
        let url = spawn_fixed_response(
            400,
            r#"{"error": "destructive operation requires confirm=true"}"#,
        )
        .await;
        let config = config_for(&tmp, url);
        let outcome = run_preview(&config, "/admin/purge-session", &preview_request()).await;
        assert!(matches!(outcome, PreviewOutcome::OlderServer));
        assert!(preview_message(&outcome, "default/scratch").is_none());
    }

    /// A 404 is surfaced: the operator asked what a purge of this session
    /// would do, and "no such session" is a real, useful answer.
    #[tokio::test]
    async fn run_preview_classifies_a_404_as_refused_and_surfaces_the_message() {
        let tmp = tempfile::TempDir::new().unwrap();
        let url = spawn_fixed_response(
            404,
            r#"{"error": "session ... not found in this workspace/project"}"#,
        )
        .await;
        let config = config_for(&tmp, url);
        let outcome = run_preview(&config, "/admin/purge-session", &preview_request()).await;
        match &outcome {
            PreviewOutcome::Refused { status, body } => {
                assert_eq!(*status, 404);
                assert!(body.contains("not found"));
            }
            _ => panic!("expected Refused, got a different outcome"),
        }
        let msg = preview_message(&outcome, "default/scratch").expect("must print a line");
        assert_eq!(
            msg,
            "Preview refused (404): session ... not found in this workspace/project"
        );
    }

    /// A successful preview (200, `dry_run: true`) is a `Previewed` outcome
    /// carrying the report through untouched.
    #[tokio::test]
    async fn run_preview_classifies_a_successful_preview() {
        let tmp = tempfile::TempDir::new().unwrap();
        let url = spawn_fixed_response(
            200,
            r#"{"session_id": "00000000-0000-0000-0000-000000000000", "workspace": "default",
                "project": "scratch", "observations_deleted": 1063, "handoffs_deleted": 0,
                "pages_deleted": 1, "auto_improve_runs_deleted": 2, "removed_paths": [],
                "collateral_observations_deleted": 0, "collateral_handoffs_denulled": 0,
                "files_deleted": [], "files_failed": [], "compacted": false, "dry_run": true}"#,
        )
        .await;
        let config = config_for(&tmp, url);
        let outcome = run_preview(&config, "/admin/purge-session", &preview_request()).await;
        let msg = preview_message(&outcome, "default/scratch").expect("must print a line");
        assert!(msg.starts_with("Would purge session from default/scratch: 1063 observations"));
    }

    /// A dropped connection must classify as `Unreachable`, the same as a
    /// timeout — both are silent fallbacks.
    #[tokio::test]
    async fn run_preview_classifies_a_dropped_connection_as_unreachable() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Own the endpoint until the request arrives; releasing a reserved
        // port first lets another concurrent test answer the preview instead.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;

            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            // Close after receiving the request, without an HTTP response.
        });
        let config = config_for(&tmp, format!("http://{addr}"));
        let outcome = run_preview(&config, "/admin/purge-session", &preview_request()).await;
        assert!(matches!(outcome, PreviewOutcome::Unreachable));
        assert!(preview_message(&outcome, "default/scratch").is_none());
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("the preview must reach the fixture")
            .unwrap();
    }
}
