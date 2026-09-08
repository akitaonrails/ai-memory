//! Per-project rolling event log.
//!
//! The log is the chronological ledger Karpathy's gist insists on — a
//! grep-able audit trail of "what happened, when". Lines use the exact
//! prefix `## [YYYY-MM-DDTHH:MM:SSZ] <event> | <title>` so unix tools
//! (`grep "^## \["`) can parse it without a markdown library.
//!
//! ## Rotation
//!
//! Each calendar month gets its own file: `log-YYYY-MM.md` under the
//! project root. The filename is derived from the event timestamp, so
//! events written across a month boundary land in their respective
//! month's file. Old months never grow once closed — perfect for git's
//! per-blob storage (one terminal blob per month per project). The
//! watcher's reserved-name check (`is_reserved_filename`) covers the
//! `log-*.md` pattern so rotated files are not indexed as wiki pages.
//!
//! No explicit retention sweep yet — git history bounds growth
//! sub-linearly and old monthly blobs are tiny. A future
//! `forget-sweep --logs --older-than 12m` could delete cold files if
//! it becomes worthwhile.

use ai_memory_core::{ProjectId, WorkspaceId};
use ai_memory_wiki::Wiki;
use jiff::{Timestamp, tz::TimeZone};
use tracing::debug;

use crate::payload::HookEvent;

/// Append one line to the current month's log file:
/// `<wiki.project_root(ws, proj)>/log-YYYY-MM.md`. The filename is
/// derived from `when` so events written across a month boundary land
/// in their respective month's file; no in-process rotation step is
/// needed.
///
/// Using [`Wiki::project_root`] ensures the path is derived from the
/// single canonical location (CLAUDE.md §15 invariant — no hand-rolled
/// path joins outside the Wiki type).
///
/// POSIX `O_APPEND` writes of less than `PIPE_BUF` (4 KiB) are atomic, so
/// concurrent appenders do not interleave.
///
/// # Errors
/// Propagates any I/O failure from opening or writing the file.
pub fn append_event(
    wiki: &Wiki,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    when: Timestamp,
    event: HookEvent,
    title: &str,
) -> std::io::Result<()> {
    let file_name = log_filename_for(when);
    let line = format_line(when, event, title);
    let path = wiki.append_under_project(workspace_id, project_id, &file_name, line.as_bytes())?;
    debug!(path = %path.display(), bytes = line.len(), "appended log entry");
    Ok(())
}

/// Compute the per-month log filename for an event with timestamp
/// `when`: `log-YYYY-MM.md` in UTC. Exposed only to the test module
/// (and the watcher's reserved-name check, which uses a glob).
pub(crate) fn log_filename_for(when: Timestamp) -> String {
    when.to_zoned(TimeZone::UTC)
        .strftime("log-%Y-%m.md")
        .to_string()
}

fn format_line(when: Timestamp, event: HookEvent, title: &str) -> String {
    let stamp = when.to_zoned(TimeZone::UTC).strftime("%Y-%m-%dT%H:%M:%SZ");
    let kind = match event {
        HookEvent::SessionStart => "session-start",
        HookEvent::UserPrompt => "user-prompt",
        HookEvent::PreToolUse => "pre-tool-use",
        HookEvent::PostToolUse => "post-tool-use",
        HookEvent::PreCompact => "pre-compact",
        HookEvent::PostCompaction => "post-compaction",
        HookEvent::Notification => "notification",
        HookEvent::Stop => "stop",
        HookEvent::SessionEnd => "session-end",
        HookEvent::SubagentStart => "subagent-start",
        HookEvent::SubagentStop => "subagent-stop",
        HookEvent::Other => "other",
    };
    let one_line: String = title
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .take(120)
        .collect();
    format!("## [{stamp}] {kind} | {one_line}\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ai_memory_store::Store;
    use ai_memory_wiki::Wiki;
    use jiff::civil::DateTime;
    use tempfile::TempDir;

    #[test]
    fn formats_line_with_expected_prefix() {
        let when: Timestamp = DateTime::new(2026, 5, 21, 12, 34, 56, 0)
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp();
        let line = format_line(when, HookEvent::SessionStart, "hello world");
        assert_eq!(
            line,
            "## [2026-05-21T12:34:56Z] session-start | hello world\n",
        );
    }

    #[tokio::test]
    async fn append_creates_file_and_grows() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
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
        let now = Timestamp::now();
        append_event(&wiki, ws, proj, now, HookEvent::SessionStart, "first").unwrap();
        append_event(&wiki, ws, proj, now, HookEvent::UserPrompt, "second").unwrap();
        let log_path = wiki.project_root(ws, proj).join(log_filename_for(now));
        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("session-start | first"));
        assert!(contents.contains("user-prompt | second"));
        // Two lines.
        assert_eq!(contents.matches("\n## [").count(), 1);
    }

    #[tokio::test]
    async fn append_rotates_across_month_boundary() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
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
        let may: Timestamp = DateTime::new(2026, 5, 31, 23, 59, 0, 0)
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp();
        let june: Timestamp = DateTime::new(2026, 6, 1, 0, 0, 5, 0)
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp();
        append_event(&wiki, ws, proj, may, HookEvent::SessionStart, "may-evt").unwrap();
        append_event(&wiki, ws, proj, june, HookEvent::UserPrompt, "june-evt").unwrap();

        let root = wiki.project_root(ws, proj);
        let may_log = std::fs::read_to_string(root.join("log-2026-05.md")).unwrap();
        let june_log = std::fs::read_to_string(root.join("log-2026-06.md")).unwrap();
        assert!(may_log.contains("may-evt"));
        assert!(!may_log.contains("june-evt"));
        assert!(june_log.contains("june-evt"));
        assert!(!june_log.contains("may-evt"));
    }

    #[test]
    fn log_filename_uses_utc_year_month() {
        let when: Timestamp = DateTime::new(2026, 1, 2, 3, 4, 5, 0)
            .unwrap()
            .to_zoned(TimeZone::UTC)
            .unwrap()
            .timestamp();
        assert_eq!(log_filename_for(when), "log-2026-01.md");
    }
}
