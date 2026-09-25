//! `ai-memory repair-backfill-timestamps` — thin HTTP client that corrects
//! `sessions.started_at`/`ended_at` for sessions an older `backfill` already
//! imported before it carried the transcript's own event time, flattening
//! every imported session onto the import day.
//!
//! Like every other lifecycle command, this is a thin client: the server
//! (`POST /admin/repair-session-times`) owns validation (including the guard
//! that only a genuinely flattened session is ever rewritten) and the write,
//! one transaction per request through the single `WriterHandle`. This
//! command's only job is the part that must run on the operator's machine —
//! reading the local harness transcripts, which the server never has access
//! to — and it reuses `backfill`'s own discovery (`collect_local_sessions`)
//! and the `ai-memory-workstream` transcript reader (`export_transcript`)
//! rather than re-parsing transcript formats.
//!
//! Dry-run by default: the server validates and reports without writing.
//! `--confirm` applies. See `docs/lifecycle-ops.md` for the full validation
//! list.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use ai_memory_core::SessionId;
use ai_memory_workstream::{ManagedHarness, build_launch_plan, export_transcript};

use super::backfill::{SessionRef, collect_local_sessions};
use super::run;
use crate::cli::RepairBackfillTimestampsArgs;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, post_json};

/// One request never carries more candidates than the server's
/// `MAX_REPAIR_SESSIONS` (`ai-memory-mcp/src/admin.rs`) accepts; a larger
/// local batch is split into several requests, each its own transaction.
const MAX_SESSIONS_PER_REQUEST: usize = 2_000;

/// One candidate posted to `POST /admin/repair-session-times`.
#[derive(Debug, Clone, Serialize)]
struct SessionTimesItem {
    session_id: String,
    started_at_us: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    ended_at_us: Option<i64>,
}

#[derive(Debug, Serialize)]
struct RepairSessionTimesRequest {
    workspace: String,
    project: String,
    sessions: Vec<SessionTimesItem>,
    confirm: bool,
}

/// The server's response, kept in full (not just a summarized subset) so
/// `--json` can pass it straight through and the human report can print
/// concrete old/new values.
#[derive(Debug, Deserialize, Serialize)]
struct RepairSessionTimesResponse {
    dry_run: bool,
    repaired: Vec<RepairedItem>,
    skipped: Vec<SkippedItem>,
}

#[derive(Debug, Deserialize, Serialize)]
struct RepairedItem {
    session_id: String,
    old_started_at_us: i64,
    #[serde(default)]
    old_ended_at_us: Option<i64>,
    new_started_at_us: i64,
    #[serde(default)]
    new_ended_at_us: Option<i64>,
    #[serde(default)]
    end_kept_open: bool,
}

#[derive(Debug, Deserialize, Serialize)]
struct SkippedItem {
    session_id: String,
    reason: String,
}

/// First/last event timestamp of one local session's transcript, in Unix
/// microseconds. `None` when the transcript could not be read or carried no
/// parseable `occurred_at` on any event.
async fn session_time_span(
    home: &Path,
    cwd: &Path,
    session_dir: Option<&Path>,
    session: &SessionRef,
) -> Option<(i64, i64)> {
    let transcript = export_transcript(
        session.harness,
        home,
        cwd,
        session_dir,
        &session.native_session_id,
        None,
    )
    .await
    .ok()?;
    let mut first_us: Option<i64> = None;
    let mut last_us: Option<i64> = None;
    for event in &transcript.events {
        let Some(us) = event
            .occurred_at
            .as_deref()
            .and_then(|s| s.parse::<jiff::Timestamp>().ok())
            .map(jiff::Timestamp::as_microsecond)
        else {
            continue;
        };
        first_us = Some(first_us.map_or(us, |f: i64| f.min(us)));
        last_us = Some(last_us.map_or(us, |l: i64| l.max(us)));
    }
    Some((first_us?, last_us?))
}

fn format_us(us: i64) -> String {
    jiff::Timestamp::from_microsecond(us)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| us.to_string())
}

/// Build one candidate per local session with a readable transcript
/// timestamp. `session_dir` is resolved once per harness (`build_launch_plan`
/// only depends on the harness, not the session), not once per session.
async fn build_candidates(
    home: &Path,
    cwd: &Path,
    sessions: &[SessionRef],
) -> (Vec<SessionTimesItem>, usize) {
    // A small, fixed set of harnesses: a linear scan avoids requiring `Hash`
    // on `ManagedHarness` for a handful of entries.
    let mut session_dirs: Vec<(ManagedHarness, Option<std::path::PathBuf>)> = Vec::new();
    let mut items = Vec::with_capacity(sessions.len());
    let mut uncaptured = 0usize;
    for session in sessions {
        let session_dir = match session_dirs.iter().find(|(h, _)| *h == session.harness) {
            Some((_, dir)) => dir.clone(),
            None => {
                let dir = build_launch_plan(session.harness, None, Vec::new(), None)
                    .ok()
                    .and_then(|plan| plan.session_dir);
                session_dirs.push((session.harness, dir.clone()));
                dir
            }
        };
        let Some((first_us, last_us)) =
            session_time_span(home, cwd, session_dir.as_deref(), session).await
        else {
            uncaptured += 1;
            continue;
        };
        let session_id = SessionId::from_native(&session.native_session_id);
        items.push(SessionTimesItem {
            session_id: session_id.to_string(),
            started_at_us: first_us,
            ended_at_us: Some(last_us),
        });
    }
    (items, uncaptured)
}

/// Run the `repair-backfill-timestamps` subcommand.
///
/// # Errors
/// Returns an error when the scope cannot be resolved, the local harness
/// session stores cannot be located, or the server is unreachable or answers
/// non-2xx.
pub async fn run(config: &Config, args: RepairBackfillTimestampsArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;
    let cwd = std::env::current_dir().context("resolving the current working directory")?;
    let home = run::native_home(config).context("locating the local harness session stores")?;

    // Same discovery `backfill` uses to select what it would import: every
    // local native session (across every scanned harness) whose recorded cwd
    // matches this checkout. Sharing it means this command looks at exactly
    // the sessions `backfill` itself would have found.
    let (sessions, limit_hit) = collect_local_sessions(&home, &cwd, None).await;
    let (items, uncaptured) = build_candidates(&home, &cwd, &sessions).await;

    if items.is_empty() {
        println!(
            "ai-memory: repair-backfill-timestamps for {workspace}/{project}: no local \
             transcript carried a readable timestamp ({uncaptured} session(s) scanned locally, \
             none captured); nothing to send."
        );
        print_scan_limit_note(&limit_hit);
        return Ok(());
    }

    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let mut responses = Vec::new();
    for chunk in items.chunks(MAX_SESSIONS_PER_REQUEST) {
        let response: RepairSessionTimesResponse = post_json(
            &endpoint,
            "/admin/repair-session-times",
            &RepairSessionTimesRequest {
                workspace: workspace.clone(),
                project: project.clone(),
                sessions: chunk.to_vec(),
                confirm: args.confirm,
            },
        )
        .await?;
        responses.push(response);
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&responses)?);
        return Ok(());
    }

    let dry_run = responses.first().is_none_or(|r| r.dry_run);
    let total_repaired: usize = responses.iter().map(|r| r.repaired.len()).sum();
    let total_skipped: usize = responses.iter().map(|r| r.skipped.len()).sum();
    let mut skipped_by_reason: BTreeMap<&str, usize> = BTreeMap::new();
    for response in &responses {
        for skip in &response.skipped {
            *skipped_by_reason.entry(skip.reason.as_str()).or_default() += 1;
        }
    }

    println!(
        "ai-memory: repair-backfill-timestamps for {workspace}/{project}: {total_repaired} \
         session(s) {}, {total_skipped} skipped, {uncaptured} local transcript(s) had no \
         readable timestamp.",
        if dry_run {
            "would be repaired"
        } else {
            "repaired"
        },
    );
    if responses.len() > 1 {
        println!(
            "  sent {} session(s) across {} request(s) of up to {MAX_SESSIONS_PER_REQUEST} each \
             (one transaction per request).",
            items.len(),
            responses.len(),
        );
    }
    for (reason, count) in &skipped_by_reason {
        println!("  skipped ({reason}): {count}");
    }
    print_scan_limit_note(&limit_hit);

    for response in &responses {
        for repaired in &response.repaired {
            println!(
                "  {}: {} .. {} -> {} .. {}{}",
                repaired.session_id,
                format_us(repaired.old_started_at_us),
                repaired
                    .old_ended_at_us
                    .map_or_else(|| "open".to_string(), format_us),
                format_us(repaired.new_started_at_us),
                repaired
                    .new_ended_at_us
                    .map_or_else(|| "open".to_string(), format_us),
                if repaired.end_kept_open {
                    " (end left open)"
                } else {
                    ""
                },
            );
        }
    }
    if dry_run && total_repaired > 0 {
        println!(
            "  dry run: nothing was written. Re-run with --confirm to apply (recorded in \
             audit_log on apply, which is the reversibility backing)."
        );
    }
    Ok(())
}

/// A harness whose local scan came back at the cap means older sessions may
/// exist that were not even considered — surface that instead of silently
/// under-reporting.
fn print_scan_limit_note(limit_hit: &[ManagedHarness]) {
    for harness in limit_hit {
        println!(
            "  note: {} returned the maximum scanned sessions for this checkout; older \
             sessions may exist and were not considered.",
            harness.as_str()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--json` aside, the human report reads real server-supplied old/new
    /// values, not values this command invented — this is a compile-time
    /// shape check on the (de)serialization, not an HTTP test (the HTTP path
    /// is covered by `admin_repair_session_times.rs` on the server side).
    #[test]
    fn repair_session_times_response_round_trips_through_json() {
        let response = RepairSessionTimesResponse {
            dry_run: true,
            repaired: vec![RepairedItem {
                session_id: "11111111-2222-3333-4444-555555555555".into(),
                old_started_at_us: 2,
                old_ended_at_us: Some(3),
                new_started_at_us: 0,
                new_ended_at_us: Some(1),
                end_kept_open: false,
            }],
            skipped: vec![SkippedItem {
                session_id: "22222222-3333-4444-5555-666666666666".into(),
                reason: "not_flattened".into(),
            }],
        };
        let json = serde_json::to_string(&response).unwrap();
        let back: RepairSessionTimesResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.repaired[0].new_started_at_us, 0);
        assert_eq!(back.skipped[0].reason, "not_flattened");
    }

    /// End-to-end proof that this command's own candidate-building reads a
    /// planted transcript's real event timestamps (not the file's mtime or
    /// any other stand-in), mirroring
    /// `backfill::tests::collect_local_sessions_finds_a_planted_claude_session_for_this_cwd`.
    /// Unix-gated for the same reason that test is: the fixture uses a
    /// POSIX-encoded `~/.claude/projects/<enc-cwd>/` layout.
    #[cfg(unix)]
    #[tokio::test]
    async fn build_candidates_reads_the_planted_transcripts_own_timestamps() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let session_dir = home
            .path()
            .join(".claude")
            .join("projects")
            .join(cwd.path().to_string_lossy().replace('/', "-"));
        std::fs::create_dir_all(&session_dir).unwrap();
        let native_id = "11111111-2222-3333-4444-555555555555";
        let header =
            serde_json::json!({"sessionId": native_id, "cwd": cwd.path().to_string_lossy()});
        let first = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "hello"},
            "timestamp": "2020-01-01T00:00:00Z",
        });
        let last = serde_json::json!({
            "type": "assistant",
            "message": {"role": "assistant", "content": "hi back"},
            "timestamp": "2020-01-01T00:05:00Z",
        });
        std::fs::write(
            session_dir.join("sess.jsonl"),
            format!("{header}\n{first}\n{last}\n"),
        )
        .unwrap();

        let (sessions, limit_hit) = crate::commands::backfill::collect_local_sessions_with(
            home.path(),
            cwd.path(),
            None,
            |_| None,
        )
        .await;
        assert!(limit_hit.is_empty(), "{limit_hit:?}");
        let (items, uncaptured) = build_candidates(home.path(), cwd.path(), &sessions).await;
        assert_eq!(uncaptured, 0, "{items:?}");
        let claude_item = items
            .iter()
            .find(|item| item.session_id == native_id)
            .unwrap_or_else(|| panic!("no candidate for {native_id} in {items:?}"));
        assert_eq!(
            claude_item.started_at_us,
            "2020-01-01T00:00:00Z"
                .parse::<jiff::Timestamp>()
                .unwrap()
                .as_microsecond()
        );
        assert_eq!(
            claude_item.ended_at_us,
            Some(
                "2020-01-01T00:05:00Z"
                    .parse::<jiff::Timestamp>()
                    .unwrap()
                    .as_microsecond()
            )
        );
    }
}
