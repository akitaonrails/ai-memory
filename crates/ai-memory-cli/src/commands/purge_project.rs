//! `ai-memory purge-project` — thin HTTP client for project purge.

use anyhow::{Result, bail};
use serde::Serialize;

use crate::cli::PurgeProjectArgs;
use crate::commands::purge_preview::{PreviewOutcome, refused_message, run_preview};
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// Request sent to `POST /admin/purge-project`.
#[derive(Serialize)]
struct PurgeProjectRequest {
    workspace: String,
    /// Exactly one of `project` / `project_id` is `Some`; the other is omitted
    /// from the wire so an older server's `project: String` still sees what it
    /// expects when the operator used the name form. (#1208)
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    /// Set instead of `project` when the operator passed `--project-id`: the
    /// server resolves the row by id, bypassing the name/legacy/canonical
    /// lookup that a collision would otherwise send to the wrong row. (#1208)
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<String>,
    confirm: bool,
    /// Purge even when a managed workstream still holds a live run lease.
    force: bool,
    /// Rebuild the FTS indexes and VACUUM after the delete commits.
    compact: bool,
    /// Preview only: wins over `confirm` on the server (mirrors
    /// `reclaim-ledger-versions`), so this is always sent alongside
    /// `confirm: false` here — never both true. Older servers that predate
    /// this field simply never look at it (an unknown JSON field is not a
    /// deserialize error), which is why the fallback below only has to
    /// handle the request failing outright, never the field being silently
    /// misread.
    dry_run: bool,
}

/// One line naming what a purge (real or previewed) removed, in the fixed
/// order the operator can grep for: pages, sessions, observations, handoffs,
/// embeddings, workstreams, managed runs. `verb` is `"Purged"` for a
/// confirmed run or `"Would purge"` for a preview — everything else about
/// the line is identical, so a script matching one also matches the other.
fn purge_summary_line(verb: &str, label: &str, report: &serde_json::Value) -> String {
    let pages = report["pages_deleted"].as_u64().unwrap_or(0);
    let sessions = report["sessions_deleted"].as_u64().unwrap_or(0);
    let observations = report["observations_deleted"].as_u64().unwrap_or(0);
    let handoffs = report["handoffs_deleted"].as_u64().unwrap_or(0);
    let embeddings = report["embeddings_deleted"].as_u64().unwrap_or(0);
    // Workstreams cascade out of the project row, so a scope that looks empty
    // by every other counter can still be carrying a managed workstream and
    // its portable event ledger. Always name them.
    let workstreams = report["workstreams_deleted"].as_u64().unwrap_or(0);
    let managed_runs = report["managed_runs_deleted"].as_u64().unwrap_or(0);
    let mut line = format!(
        "{verb} {label}: {pages} pages, {sessions} sessions, \
         {observations} observations, {handoffs} handoffs, {embeddings} embeddings, \
         {workstreams} workstreams, {managed_runs} managed runs."
    );
    // The mirror of the incident this command guards against: purging this
    // scope also collaterally deletes/orphans rows that live in *other*
    // projects, via `sessions` cascading out of this one. Silent when zero
    // so the common case reads exactly as before.
    let collateral_observations = report["collateral_observations_deleted"]
        .as_u64()
        .unwrap_or(0);
    if collateral_observations > 0 {
        line.push_str(&format!(
            " Plus {collateral_observations} observations in other projects via their sessions."
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

/// The exact command that performs the purge the refusal just previewed.
///
/// Rebuilt from `args` rather than hand-written, the same way
/// `move_session::apply_command` does, so the printed command cannot drift
/// from the request the preview was computed with. Every flag the operator
/// passed has to reappear here: the preview above was computed WITH
/// `force`/`compact`, so a command that silently dropped them would not
/// reproduce the operation the operator was just shown — and a dropped
/// `--force` means the server answers the pasted command with a 409 `managed
/// run lease is active`, which is the exact condition `--force` exists to
/// override.
fn confirm_command(workspace: &str, target_flag: &str, args: &PurgeProjectArgs) -> String {
    let workspace = super::render_shared::native_shell_quote(workspace);
    let mut cmd = format!("ai-memory purge-project --workspace {workspace} {target_flag}");
    if args.force {
        cmd.push_str(" --force");
    }
    if args.compact {
        cmd.push_str(" --compact");
    }
    cmd.push_str(" --confirm");
    cmd
}

/// Refuse a confirmed purge whose target nobody typed.
///
/// `--confirm` is the operator's "yes, this project". When neither
/// `--project` nor `--project-id` was passed, the target came from the
/// marker, the git remote or the directory name, so the same command run from
/// the wrong directory deleted a different project, and nothing named it
/// before the delete. The refusal names the derived target and hands back the
/// command with it spelled out, so a deliberate purge is one paste away.
fn require_named_target_for_confirm(
    args: &PurgeProjectArgs,
    workspace: &str,
    target_label: &str,
    target_flag: &str,
) -> Result<()> {
    if !args.confirm || args.project.is_some() || args.project_id.is_some() {
        return Ok(());
    }
    bail!(
        "purge-project --confirm needs the project named with --project or --project-id; \
         this directory resolves to {workspace}/{target_label} (from the .ai-memory.toml \
         marker, the git remote or the directory name), and nothing was deleted.\n\
         If that is the project to purge, re-run:\n\n  {}",
        confirm_command(workspace, target_flag, args),
    )
}

/// Decide what to print, if anything, before the refusal — pure, so it is
/// unit-tested without a server. `fallback_label` is used only when the
/// server's own report has no `label` field.
fn preview_message(outcome: &PreviewOutcome, fallback_label: &str) -> Option<String> {
    match outcome {
        PreviewOutcome::Previewed(report) => {
            let label = report["label"].as_str().unwrap_or(fallback_label);
            Some(purge_summary_line("Would purge", label, report))
        }
        PreviewOutcome::Refused { status, body } => Some(refused_message(*status, body)),
        PreviewOutcome::Ignored | PreviewOutcome::OlderServer | PreviewOutcome::Unreachable => None,
    }
}

/// Run the `purge-project` subcommand.
///
/// Resolves the project name (auto-derived from the git repo root when
/// `--project` is omitted) -- or, when `--project-id` is given, skips name
/// resolution and targets that id directly, which is the only way to reach a
/// legacy row whose name is shadowed by another project's `legacy_name` --
/// requires `--confirm` before sending the destructive request, then prints
/// the JSON summary.
///
/// Without `--confirm`, first asks the server for a preview (`dry_run:
/// true`, which wins over `confirm` server-side): the server reports the
/// counts a confirmed purge would produce without deleting anything. That
/// preview is best-effort, bounded by
/// [`purge_preview::PREVIEW_TIMEOUT`](crate::commands::purge_preview::PREVIEW_TIMEOUT),
/// and never changes the outcome — only what gets printed before it:
/// - a successful preview prints the "Would purge ..." line;
/// - a 404/409/403 (or any other unexpected status) prints the server's own
///   error first, since the operator asked what would happen and the server
///   has an answer, just not the one this command expected;
/// - a plain 400 (an older server that predates `dry_run`), a timeout, or an
///   unreachable server print nothing extra — the refusal below already
///   says everything a 400 would.
///
/// # Errors
/// Returns an error when `--confirm` is absent (after printing whatever the
/// preview surfaced), the server is unreachable, or the server returns a
/// non-2xx response.
pub async fn run(config: &Config, args: PurgeProjectArgs) -> Result<()> {
    // An id target skips name resolution entirely: `resolve_scope` would
    // auto-derive a project name from the CWD, which is exactly the ambiguity
    // `--project-id` exists to sidestep. The workspace still resolves the
    // normal way (marker, else `default`), since the server needs it to scope
    // the id lookup.
    let (workspace, project, project_id) = match &args.project_id {
        Some(id) => (
            super::resolve_workspace(config, args.workspace.as_deref())?,
            None,
            Some(id.clone()),
        ),
        None => {
            let (ws, project) =
                super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
            (ws, Some(project), None)
        }
    };
    // The flag that names the target, for the refusal's "re-run with" line and
    // the preview's fallback label. A `--project-id` run has no resolved name
    // to print, so the id stands in.
    let (target_flag, target_label) = match (&project, &project_id) {
        (Some(project), _) => (
            format!(
                "--project {}",
                super::render_shared::native_shell_quote(project)
            ),
            project.clone(),
        ),
        (None, Some(id)) => (
            format!(
                "--project-id {}",
                super::render_shared::native_shell_quote(id)
            ),
            id.clone(),
        ),
        (None, None) => bail!("one of --project or --project-id is required"),
    };
    require_named_target_for_confirm(&args, &workspace, &target_label, &target_flag)?;

    if !args.confirm {
        let request = PurgeProjectRequest {
            workspace: workspace.clone(),
            project: project.clone(),
            project_id: project_id.clone(),
            confirm: false,
            force: args.force,
            compact: args.compact,
            dry_run: true,
        };
        let outcome = run_preview(config, "/admin/purge-project", &request).await;
        let fallback_label = format!("{}/{}", workspace, target_label);
        if let Some(line) = preview_message(&outcome, &fallback_label) {
            println!("{line}");
        }
        bail!(
            "purge-project is destructive and irreversible.\n\
             Re-run with --confirm to proceed:\n\n  {}",
            confirm_command(&workspace, &target_flag, &args),
        );
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let report: serde_json::Value = post_json(
        &endpoint,
        "/admin/purge-project",
        &PurgeProjectRequest {
            workspace: workspace.clone(),
            project: project.clone(),
            project_id: project_id.clone(),
            confirm: true,
            force: args.force,
            compact: args.compact,
            dry_run: false,
        },
    )
    .await?;

    // Human-friendly one-liner followed by the raw JSON for scripting.
    let fallback_label = format!("{}/{}", workspace, target_label);
    let label = report["label"].as_str().unwrap_or(&fallback_label);
    println!("{}", purge_summary_line("Purged", label, &report));
    if let Some(ids) = report["workstream_ids"].as_array()
        && !ids.is_empty()
    {
        println!(
            "The following workstream segment directories are now orphaned under \
             <data_dir>/raw/workstreams/ and can be removed:"
        );
        for id in ids.iter().filter_map(serde_json::Value::as_str) {
            println!("  - {id}");
        }
    }
    if let Some(failed) = report["files_failed"].as_array()
        && !failed.is_empty()
    {
        println!(
            "Warning: {} wiki file(s) could not be removed from disk (DB rows are gone).",
            failed.len()
        );
    }
    if report["compacted"].as_bool().unwrap_or(false) {
        println!("Database compacted: freed bytes reclaimed.");
    } else {
        println!(
            "Note: this was a logical delete. The project is unreachable through \
             the API and search, but its bytes remain in the database file until \
             it is rewritten. Re-run with --compact to reclaim them (slow), and \
             see docs/lifecycle-ops.md for what that does and does not guarantee."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(dry_run: bool) -> serde_json::Value {
        serde_json::json!({
            "label": "default/my-project",
            "pages_deleted": 3,
            "sessions_deleted": 1,
            "observations_deleted": 1063,
            "handoffs_deleted": 0,
            "embeddings_deleted": 3,
            "collateral_observations_deleted": 0,
            "collateral_handoffs_denulled": 0,
            "workstreams_deleted": 0,
            "managed_runs_deleted": 0,
            "workstream_ids": [],
            "files_deleted": [],
            "files_failed": [],
            "compacted": false,
            "dry_run": dry_run,
        })
    }

    fn args() -> PurgeProjectArgs {
        PurgeProjectArgs {
            force: false,
            workspace: Some("default".into()),
            project: Some("my-project".into()),
            project_id: None,
            confirm: false,
            compact: false,
        }
    }

    /// The command the refusal hands the operator must round-trip every flag
    /// they passed. The preview printed just above it was computed WITH
    /// `force`, so a command missing `--force` does not reproduce the
    /// operation the operator was shown — it gets a 409 `managed run lease is
    /// active` from the server, on exactly the condition `--force` suppresses.
    #[test]
    fn confirm_command_repeats_force_and_compact() {
        let mut a = args();
        a.force = true;
        a.compact = true;
        assert_eq!(
            confirm_command("default", "--project my-project", &a),
            "ai-memory purge-project --workspace default --project my-project \
             --force --compact --confirm"
        );
    }

    /// And the mirror: a run with neither flag must not start emitting flags
    /// nobody asked for — pasting `--force` into a plain purge would silently
    /// widen it to delete a live run's lease.
    #[test]
    fn confirm_command_omits_flags_the_operator_did_not_pass() {
        let mut a = args();
        a.project = None;
        a.project_id = Some("0192b6a1-4c2e-7d3f-8a5b-1234567890ab".into());
        assert_eq!(
            confirm_command(
                "default",
                "--project-id 0192b6a1-4c2e-7d3f-8a5b-1234567890ab",
                &a
            ),
            "ai-memory purge-project --workspace default \
             --project-id 0192b6a1-4c2e-7d3f-8a5b-1234567890ab --confirm"
        );
    }

    /// A confirmed purge of a target nobody typed is refused, and the refusal
    /// names the derived target and hands back the command with it spelled
    /// out, every flag the operator passed included.
    #[test]
    fn confirm_without_a_named_target_is_refused_with_the_spelled_out_command() {
        let mut a = args();
        a.project = None;
        a.confirm = true;
        a.force = true;
        let err = require_named_target_for_confirm(
            &a,
            "default",
            "derived-from-cwd",
            "--project derived-from-cwd",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("resolves to default/derived-from-cwd"),
            "{err}"
        );
        assert!(err.contains("nothing was deleted"), "{err}");
        assert!(
            err.ends_with(
                "ai-memory purge-project --workspace default --project derived-from-cwd \
                 --force --confirm"
            ),
            "{err}"
        );
    }

    /// Controls: a named target confirms as before, and the preview path
    /// (no `--confirm`) still works from the derived name alone.
    #[test]
    fn a_named_target_or_a_preview_is_not_refused() {
        let mut named = args();
        named.confirm = true;
        assert!(
            require_named_target_for_confirm(
                &named,
                "default",
                "my-project",
                "--project my-project"
            )
            .is_ok()
        );

        let mut by_id = args();
        by_id.project = None;
        by_id.project_id = Some("0192b6a1-4c2e-7d3f-8a5b-1234567890ab".into());
        by_id.confirm = true;
        assert!(
            require_named_target_for_confirm(
                &by_id,
                "default",
                "0192b6a1-4c2e-7d3f-8a5b-1234567890ab",
                "--project-id 0192b6a1-4c2e-7d3f-8a5b-1234567890ab"
            )
            .is_ok()
        );

        let mut preview = args();
        preview.project = None;
        assert!(
            require_named_target_for_confirm(&preview, "default", "derived", "--project derived")
                .is_ok()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn confirm_command_quotes_free_form_scope_names() {
        assert_eq!(
            confirm_command("team space", "--project 'api; echo nope'", &args()),
            "ai-memory purge-project --workspace 'team space' --project 'api; echo nope' --confirm"
        );
    }

    /// Pins the exact wording and field order a script would grep for.
    /// `purge_summary_line` is the single source for both the confirmed
    /// "Purged" line and the preview's "Would purge" line, so this also
    /// proves the two can never drift apart.
    #[test]
    fn purge_summary_line_matches_the_documented_wording() {
        let confirmed = report(false);
        assert_eq!(
            purge_summary_line("Purged", "default/my-project", &confirmed),
            "Purged default/my-project: 3 pages, 1 sessions, 1063 observations, \
             0 handoffs, 3 embeddings, 0 workstreams, 0 managed runs."
        );

        let preview = report(true);
        assert_eq!(
            purge_summary_line("Would purge", "default/my-project", &preview),
            "Would purge default/my-project: 3 pages, 1 sessions, 1063 observations, \
             0 handoffs, 3 embeddings, 0 workstreams, 0 managed runs."
        );
    }

    /// Missing counters must not panic and must not silently show as
    /// non-zero: a malformed or truncated reply reads as all-zero, which the
    /// operator can visibly tell apart from a real "0 pages, 0 sessions".
    #[test]
    fn purge_summary_line_defaults_missing_counters_to_zero() {
        let empty = serde_json::json!({});
        assert_eq!(
            purge_summary_line("Would purge", "default/x", &empty),
            "Would purge default/x: 0 pages, 0 sessions, 0 observations, \
             0 handoffs, 0 embeddings, 0 workstreams, 0 managed runs."
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
            line.contains("Plus 7 observations in other projects via their sessions."),
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
        let msg = preview_message(&outcome, "default/fallback").expect("must print a line");
        assert!(msg.starts_with("Would purge default/my-project:"));
    }

    /// A 404 (unknown scope) is worth showing before the refusal: the
    /// operator asked what would happen, and 404 is the server's answer.
    #[test]
    fn preview_message_surfaces_a_404_before_the_refusal() {
        let outcome = PreviewOutcome::Refused {
            status: 404,
            body: r#"{"error":"project 'ghost' not found in workspace 'default'"}"#.to_string(),
        };
        let msg = preview_message(&outcome, "default/ghost").expect("must print a line");
        assert_eq!(
            msg,
            "Preview refused (404): project 'ghost' not found in workspace 'default'"
        );
    }

    /// A 409 (live managed run) is the same: surfaced, not swallowed.
    #[test]
    fn preview_message_surfaces_a_409_before_the_refusal() {
        let outcome = PreviewOutcome::Refused {
            status: 409,
            body: r#"{"error":"managed run lease is active for 'main' (claude-code)"}"#.to_string(),
        };
        let msg = preview_message(&outcome, "default/x").expect("must print a line");
        assert_eq!(
            msg,
            "Preview refused (409): managed run lease is active for 'main' (claude-code)"
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
    /// None of these should print anything — the refusal that follows in
    /// `run` already says everything a 400 would, and there is nothing
    /// useful to say about a request that never got an answer.
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
            "/admin/purge-project",
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

    fn preview_request() -> PurgeProjectRequest {
        PurgeProjectRequest {
            workspace: "default".into(),
            project: Some("scratch".into()),
            project_id: None,
            confirm: false,
            force: false,
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
        let outcome = run_preview(&config, "/admin/purge-project", &preview_request()).await;
        assert!(matches!(outcome, PreviewOutcome::OlderServer));
        assert!(preview_message(&outcome, "default/scratch").is_none());
    }

    /// A 404 is surfaced: the operator asked what a purge of this scope
    /// would do, and "no such scope" is a real, useful answer.
    #[tokio::test]
    async fn run_preview_classifies_a_404_as_refused_and_surfaces_the_message() {
        let tmp = tempfile::TempDir::new().unwrap();
        let url = spawn_fixed_response(
            404,
            r#"{"error": "project 'scratch' not found in workspace 'default'"}"#,
        )
        .await;
        let config = config_for(&tmp, url);
        let outcome = run_preview(&config, "/admin/purge-project", &preview_request()).await;
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
            "Preview refused (404): project 'scratch' not found in workspace 'default'"
        );
    }

    /// A successful preview (200, `dry_run: true`) is a `Previewed` outcome
    /// carrying the report through untouched.
    #[tokio::test]
    async fn run_preview_classifies_a_successful_preview() {
        let tmp = tempfile::TempDir::new().unwrap();
        let url = spawn_fixed_response(
            200,
            r#"{"label": "default/scratch", "pages_deleted": 3, "sessions_deleted": 1,
                "observations_deleted": 1063, "handoffs_deleted": 0, "embeddings_deleted": 3,
                "collateral_observations_deleted": 0, "collateral_handoffs_denulled": 0,
                "workstreams_deleted": 0, "managed_runs_deleted": 0, "workstream_ids": [],
                "files_deleted": [], "files_failed": [], "compacted": false, "dry_run": true}"#,
        )
        .await;
        let config = config_for(&tmp, url);
        let outcome = run_preview(&config, "/admin/purge-project", &preview_request()).await;
        let msg = preview_message(&outcome, "default/scratch").expect("must print a line");
        assert!(msg.starts_with("Would purge default/scratch: 3 pages, 1 sessions, 1063"));
    }

    /// End to end: the refusal happens before any request, so a server that
    /// would accept the purge never sees one.
    #[tokio::test]
    async fn confirm_without_a_named_target_sends_no_request() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        let app = axum::Router::new().route(
            "/admin/purge-project",
            axum::routing::post(move || {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { axum::Json(serde_json::json!({ "label": "default/x" })) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let tmp = tempfile::TempDir::new().unwrap();
        let config = config_for(&tmp, format!("http://{addr}"));
        let mut a = args();
        a.project = None;
        a.confirm = true;

        let err = run(&config, a).await.unwrap_err().to_string();

        assert!(err.contains("needs the project named"), "{err}");
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// Nothing listening at all (connection refused) must classify as
    /// `Unreachable`, the same as a timeout — both are silent fallbacks.
    #[tokio::test]
    async fn run_preview_classifies_a_connection_failure_as_unreachable() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Bind then drop immediately: the port is very likely free again by
        // the time the request lands, and nothing else can be listening on
        // it inside this test's short lifetime.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let config = config_for(&tmp, format!("http://{addr}"));
        let outcome = run_preview(&config, "/admin/purge-project", &preview_request()).await;
        assert!(matches!(outcome, PreviewOutcome::Unreachable));
        assert!(preview_message(&outcome, "default/scratch").is_none());
    }
}
