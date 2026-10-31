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

/// Warning printed when `--confirm` is used without naming the target.
///
/// The derived name is correct only when the operator is standing in the
/// checkout they mean. `--confirm` typed from the wrong directory deletes
/// that directory's project, and nothing in the request distinguishes the two
/// cases, so the command names what it resolved and how to say it explicitly
/// (#1642). Enforcing the explicit name outright would remove an existing
/// invocation, so this stays a deprecation until the next major.
fn derived_target_warning(
    workspace: &str,
    target_flag: &str,
    target_label: &str,
    args: &PurgeProjectArgs,
) -> String {
    format!(
        "warning: purge-project --confirm without --project or --project-id is deprecated and \
         will be refused in ai-memory 4.0.\n\
         This directory resolves to {workspace}/{target_label}\n\n\
         If that is the project to purge, name it:\n\n  {}",
        confirm_command(workspace, target_flag, args),
    )
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
    let i18n = &config.i18n;
    super::refuse_empty_explicit(args.workspace.as_deref(), "--workspace")?;
    super::refuse_empty_explicit(args.project.as_deref(), "--project")?;
    super::refuse_empty_explicit(args.project_id.as_deref(), "--project-id")?;
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
        (None, None) => bail!(
            "{}",
            i18n.text("cli-move-project-one-of-project-or-project-id-is-required")
        ),
    };

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
            "{}", i18n.format("cli-purge-project-purge-project-is-destructive-and-irreversible-re-run-with-confirm-to-proceed", &[("value0", confirm_command(&workspace, &target_flag, &args).to_string())]));
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    // The name was derived, not typed: say so before the irreparable step, and
    // name the exact command that would have been unambiguous (#1642).
    if args.project.is_none() && args.project_id.is_none() {
        eprintln!(
            "{}",
            derived_target_warning(&workspace, &target_flag, &target_label, &args)
        );
    }
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
            "{}", i18n.text("cli-purge-project-the-following-workstream-segment-directories-are-now-orphaned-under-data-dir"));
        for id in ids.iter().filter_map(serde_json::Value::as_str) {
            println!("  - {id}");
        }
    }
    if let Some(failed) = report["files_failed"].as_array()
        && !failed.is_empty()
    {
        println!(
            "{}", i18n.format("cli-purge-project-warning-wiki-file-s-could-not-be-removed-from-disk-db-rows-are-gone", &[("value0", format!("{}", failed.len()))]));
    }
    if report["compacted"].as_bool().unwrap_or(false) {
        println!(
            "{}",
            i18n.text("cli-purge-project-database-compacted-freed-bytes-reclaimed")
        );
    } else {
        println!(
            "{}", i18n.text("cli-purge-project-note-this-was-a-logical-delete-the-project-is-unreachable-through-the-api-an"));
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
        let outcome = run_preview(&config, "/admin/purge-project", &preview_request()).await;
        assert!(matches!(outcome, PreviewOutcome::Unreachable));
        assert!(preview_message(&outcome, "default/scratch").is_none());
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("the preview must reach the fixture")
            .unwrap();
    }

    // -----------------------------------------------------------------
    // #1642: `--confirm` without a named target.
    //
    // Making the name mandatory removes an invocation a script may rely on,
    // so this ships as a deprecation: the purge still happens, and it now says
    // what it resolved and how to name it. An explicitly empty flag, which
    // silently resolved to the same derived target, is refused outright.
    // -----------------------------------------------------------------

    #[test]
    fn derived_target_warning_names_the_resolved_project_and_the_command() {
        let mut a = args();
        a.project = None;
        a.confirm = true;
        let warning = derived_target_warning("default", "--project api", "api", &a);
        assert!(
            warning.contains("deprecated and will be refused in ai-memory 4.0"),
            "the warning must say when it starts refusing: {warning}"
        );
        assert!(
            warning.contains("This directory resolves to default/api"),
            "the warning must name the target the delete would reach: {warning}"
        );
        assert!(
            warning.contains("ai-memory purge-project --workspace default --project api --confirm"),
            "the warning must hand over the unambiguous command: {warning}"
        );
    }

    /// The empty string is the whole bypass: `resolve_scope` reads it as
    /// absent, so `--project ""` used to purge the derived target while
    /// looking like a named run. Nothing may reach the wire.
    #[tokio::test]
    async fn an_empty_project_flag_is_refused_before_any_request() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (url, hits) = spawn_counting_server().await;
        let mut a = args();
        a.project = Some(String::new());
        a.project_id = None;
        a.confirm = true;
        let error = run(&config_for(&tmp, url), a).await.unwrap_err();
        assert!(
            error.to_string().contains("--project was passed empty"),
            "unexpected error: {error}"
        );
        assert_eq!(
            *hits.lock().await,
            0,
            "the refusal must precede the request"
        );
    }

    /// The behaviour this release keeps: a confirmed purge whose target came
    /// from the checkout still runs, and lands on the derived project.
    #[tokio::test]
    async fn a_confirmed_purge_with_a_derived_target_still_purges() {
        let tmp = tempfile::TempDir::new().unwrap();
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::write(checkout.join(".ai-memory.toml"), "project = \"scratch\"\n").unwrap();
        let (url, hits) = spawn_counting_server().await;
        let config = Config {
            data_dir: tmp.path().to_path_buf(),
            server_url: url,
            runtime_env: crate::config::RuntimeEnv::with_host_cwd_for_tests(
                checkout.to_str().unwrap(),
            ),
            ..Config::default()
        };
        let mut a = args();
        a.project = None;
        a.workspace = None;
        a.confirm = true;
        run(&config, a).await.unwrap();
        assert_eq!(*hits.lock().await, 1, "the confirmed purge must be sent");
    }

    /// Serves the purge report and counts the requests that reached it.
    async fn spawn_counting_server() -> (String, std::sync::Arc<tokio::sync::Mutex<u32>>) {
        let hits = std::sync::Arc::new(tokio::sync::Mutex::new(0_u32));
        let counter = std::sync::Arc::clone(&hits);
        let app = axum::Router::new().route(
            "/admin/purge-project",
            axum::routing::post(move || {
                let counter = std::sync::Arc::clone(&counter);
                async move {
                    *counter.lock().await += 1;
                    axum::Json(serde_json::json!({
                        "label": "default/scratch",
                        "pages_deleted": 1, "sessions_deleted": 0, "observations_deleted": 0,
                        "handoffs_deleted": 0, "embeddings_deleted": 0,
                        "collateral_observations_deleted": 0, "collateral_handoffs_denulled": 0,
                        "workstreams_deleted": 0, "managed_runs_deleted": 0,
                        "workstream_ids": [], "files_deleted": [], "files_failed": [],
                        "compacted": false, "dry_run": false,
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), hits)
    }
}
