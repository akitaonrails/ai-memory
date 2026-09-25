//! `ai-memory doctor` — capture-coverage check.
//!
//! Capture is silently gated on each harness having an ai-memory hook
//! installed: a harness with no hook still writes its own local session
//! transcripts, but nothing reaches the server, and nothing today reconciles
//! "this harness ran in this project" against "this harness captured nothing".
//! An operator who believes all their harnesses feed one memory is then
//! quietly wrong (audit finding F1 — Kimi ran on a project for weeks with no
//! hook and zero captured sessions).
//!
//! This command makes that gap visible. For the current project it enumerates
//! the local native session stores of every known harness (reusing the
//! read-only workstream adapters, so the per-harness path encoding lives in
//! exactly one place), asks the server how many sessions it captured per agent
//! (`GET /admin/sessions/by-agent`), and warns when a harness has recent local
//! sessions here but zero captured ones — with the exact `install-hooks`
//! command to close the gap.
//!
//! It never opens the store directly; the server remains the source of truth
//! for the captured side, and the local side is read-only.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use ai_memory_core::AgentKind;
use ai_memory_workstream::{
    LaunchRoots, ManagedHarness, build_launch_plan_with_env, list_native_sessions,
};

use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Every harness with a read-only native-session adapter. Variants that share
/// an [`AgentKind`] (Kiro v2/v3, OpenCode v1/v2) are both scanned — they read
/// distinct on-disk stores — and their local counts fold together under the
/// one agent kind the server records.
pub(crate) const SCANNED_HARNESSES: &[ManagedHarness] = &[
    ManagedHarness::Claude,
    ManagedHarness::Codex,
    ManagedHarness::OpenCode,
    ManagedHarness::OpenCode2,
    ManagedHarness::Pi,
    ManagedHarness::Crush,
    ManagedHarness::Omp,
    ManagedHarness::Kimi,
    ManagedHarness::CommandCode,
    ManagedHarness::Kiro,
    ManagedHarness::KiroV3,
    ManagedHarness::Grok,
    ManagedHarness::Antigravity,
];

/// Cap the per-harness enumeration. A project with more local sessions than
/// this for one harness is already captured or not; the exact count past the
/// cap does not change any verdict.
const SCAN_LIMIT: usize = 1_000;

/// One agent kind's local (on-disk) session tally for the current project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocalScan {
    agent: AgentKind,
    /// Local native sessions for this cwd across every harness of this kind.
    total: usize,
    /// Of those, how many were updated within the "recent" window.
    recent: usize,
}

/// A per-agent capture-coverage verdict for the current project.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CoverageRow {
    agent: String,
    local_total: usize,
    local_recent: usize,
    captured: u64,
    /// True when the harness ran here recently but captured nothing — the
    /// high-confidence "hook is missing" signal.
    uncaptured: bool,
}

impl CoverageRow {
    /// The single, deliberately conservative verdict: a harness that produced
    /// recent local sessions in this project yet has zero captured sessions is
    /// almost certainly missing its hook. Anything else is left un-flagged —
    /// one captured session proves the hook works, and a purely historical
    /// local store (nothing recent) is not actionable. Session-id reuse across
    /// `--resume` means N local files can legitimately map to one server
    /// session, so a partial (nonzero-but-fewer) count is never treated as a
    /// gap.
    fn compute_uncaptured(local_recent: usize, captured: u64) -> bool {
        local_recent > 0 && captured == 0
    }
}

/// The captured side: `GET /admin/sessions/by-agent` returns per-agent session
/// counts keyed by the stored kebab-case agent kind (`AgentKind::as_str`).
#[derive(Debug, Deserialize)]
struct ByAgentResponse {
    by_agent: Vec<AgentCount>,
}

#[derive(Debug, Deserialize)]
struct AgentCount {
    agent: String,
    sessions: u64,
}

/// The full report, also the JSON output shape.
#[derive(Debug, Serialize)]
struct DoctorReport {
    workspace: String,
    project: String,
    server: String,
    since_days: u32,
    rows: Vec<CoverageRow>,
    /// The agent kinds (kebab form) flagged as uncaptured, for quick scripting.
    uncaptured: Vec<String>,
}

/// Fold the raw per-harness local scans and the server's captured counts into
/// one row per agent kind. Pure: no IO, so the verdict logic is unit-tested
/// directly. Rows for agent kinds that neither ran locally nor captured
/// anything are dropped — only harnesses that touched this project are worth
/// showing.
pub(crate) fn build_rows(
    local: &[LocalScan],
    captured: &BTreeMap<String, u64>,
) -> Vec<CoverageRow> {
    // Aggregate local scans by agent kind (Kiro v2/v3, OpenCode v1/v2 collapse).
    let mut by_agent: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for scan in local {
        let entry = by_agent.entry(scan.agent.as_str().to_string()).or_default();
        entry.0 += scan.total;
        entry.1 += scan.recent;
    }

    // Union of "ran locally" and "captured on the server".
    let mut agents: Vec<String> = by_agent.keys().cloned().collect();
    for agent in captured.keys() {
        if !by_agent.contains_key(agent) {
            agents.push(agent.clone());
        }
    }
    agents.sort();
    agents.dedup();

    let mut rows: Vec<CoverageRow> = agents
        .into_iter()
        .map(|agent| {
            let (local_total, local_recent) = by_agent.get(&agent).copied().unwrap_or((0, 0));
            let captured = captured.get(&agent).copied().unwrap_or(0);
            let uncaptured = CoverageRow::compute_uncaptured(local_recent, captured);
            CoverageRow {
                agent,
                local_total,
                local_recent,
                captured,
                uncaptured,
            }
        })
        .filter(|row| row.local_total > 0 || row.captured > 0)
        .collect();

    // Surface the actionable warnings first, then a stable alphabetical order.
    rows.sort_by(|a, b| {
        b.uncaptured
            .cmp(&a.uncaptured)
            .then_with(|| a.agent.cmp(&b.agent))
    });
    rows
}

/// Enumerate local native sessions for `cwd` across every scanned harness.
/// Read-only. A harness whose store is unreadable, absent, or unsupported
/// simply contributes nothing — the command never invents a gap it cannot see.
pub(crate) async fn scan_local(home: &Path, cwd: &Path, since_days: u32) -> Vec<LocalScan> {
    let recent_cutoff = recent_cutoff(SystemTime::now(), since_days);
    let mut scans = Vec::new();
    for &harness in SCANNED_HARNESSES {
        // Honor harness home relocations (CODEX_HOME, KIMI_CODE_HOME, …) via the
        // same launch-plan resolver `ai-memory run` uses; fall back to the
        // default $HOME-relative store when a probe plan cannot be built.
        let roots = LaunchRoots { home, cwd };
        let session_dir =
            build_launch_plan_with_env(harness, None, Vec::new(), None, &[], Some(roots))
                .ok()
                .and_then(|plan| plan.session_dir);
        let Ok(sessions) =
            list_native_sessions(harness, home, cwd, session_dir.as_deref(), SCAN_LIMIT).await
        else {
            continue;
        };
        if sessions.is_empty() {
            continue;
        }
        let recent = sessions
            .iter()
            .filter(|s| is_recent(s.updated_at, recent_cutoff))
            .count();
        scans.push(LocalScan {
            agent: harness.agent_kind(),
            total: sessions.len(),
            recent,
        });
    }
    scans
}

/// The lower time bound for "recent"; `None` means every session counts
/// (`--since-days 0`).
fn recent_cutoff(now: SystemTime, since_days: u32) -> Option<SystemTime> {
    if since_days == 0 {
        return None;
    }
    now.checked_sub(Duration::from_secs(u64::from(since_days) * 86_400))
}

fn is_recent(updated_at: SystemTime, cutoff: Option<SystemTime>) -> bool {
    match cutoff {
        None => true,
        Some(cutoff) => updated_at >= cutoff,
    }
}

/// Run the capture-coverage doctor.
///
/// # Errors
/// Returns an error when the current scope cannot be resolved, the working
/// directory is unreadable, or the server cannot be reached / its response
/// cannot be parsed.
pub async fn run(config: &Config, args: crate::cli::DoctorArgs) -> Result<()> {
    let (workspace, project) =
        super::resolve_scope(config, args.workspace.as_deref(), args.project.as_deref())?;

    let cwd = std::env::current_dir().context("resolving the current working directory")?;
    let home =
        super::run::native_home(config).context("locating the local harness session stores")?;

    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    let captured: BTreeMap<String, u64> = match get_json::<ByAgentResponse>(
        &ep,
        "/admin/sessions/by-agent",
        &[
            ("workspace", workspace.as_str()),
            ("project", project.as_str()),
        ],
    )
    .await
    {
        Ok(response) => response
            .by_agent
            .into_iter()
            .map(|c| (c.agent, c.sessions))
            .collect(),
        // A project that has never been captured into does not exist
        // server-side yet (404) — that is "nothing captured", not an error, so
        // every local harness correctly reads as uncaptured.
        Err(error) if super::is_scope_not_found(&error) => BTreeMap::new(),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("asking the server for captured session counts for {workspace}/{project}")
            });
        }
    };

    let local = scan_local(&home, &cwd, args.since_days).await;
    let rows = build_rows(&local, &captured);
    let uncaptured: Vec<String> = rows
        .iter()
        .filter(|r| r.uncaptured)
        .map(|r| r.agent.clone())
        .collect();

    let report = DoctorReport {
        workspace,
        project,
        server: ep.url.clone(),
        since_days: args.since_days,
        rows,
        uncaptured,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        render_human(&report);
    }
    Ok(())
}

fn render_human(report: &DoctorReport) {
    println!(
        "Capture coverage for {}/{} (server {})",
        report.workspace, report.project, report.server
    );
    if report.since_days == 0 {
        println!("  recent window: all on-disk sessions\n");
    } else {
        println!("  recent window: last {} days\n", report.since_days);
    }

    if report.rows.is_empty() {
        println!("  No local harness sessions found for this project and nothing captured yet.");
        return;
    }

    for row in &report.rows {
        let mark = if row.uncaptured { "⚠" } else { "✓" };
        println!(
            "  {mark} {:<14} {:>4} local ({} recent) → {:>4} captured",
            row.agent, row.local_total, row.local_recent, row.captured
        );
        if row.uncaptured {
            println!(
                "      └ ran here but nothing was captured — install its hook:\n        \
                 ai-memory install-hooks --agent {} --apply",
                row.agent
            );
        }
    }

    if report.uncaptured.is_empty() {
        println!("\n✓ Every harness that ran in this project recently is captured on the server.");
    } else {
        println!(
            "\n⚠ {} harness(es) ran here recently with no captured sessions: {}.\n  \
             Install the hook(s) above, then re-run `ai-memory doctor` to confirm.",
            report.uncaptured.len(),
            report.uncaptured.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(agent: AgentKind, total: usize, recent: usize) -> LocalScan {
        LocalScan {
            agent,
            total,
            recent,
        }
    }

    fn captured(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
        pairs.iter().map(|(a, n)| ((*a).to_string(), *n)).collect()
    }

    fn row<'a>(rows: &'a [CoverageRow], agent: &str) -> &'a CoverageRow {
        rows.iter()
            .find(|r| r.agent == agent)
            .unwrap_or_else(|| panic!("expected a row for {agent}; got {rows:?}"))
    }

    #[test]
    fn recent_local_with_zero_captured_is_the_only_flagged_case() {
        // Kimi ran here recently but nothing was captured — the F1 gap.
        let local = vec![scan(AgentKind::KimiCode, 2, 2)];
        let rows = build_rows(&local, &captured(&[]));
        let kimi = row(&rows, "kimi-code");
        assert!(kimi.uncaptured, "recent local + zero captured must warn");
        assert_eq!(kimi.local_recent, 2);
        assert_eq!(kimi.captured, 0);
    }

    #[test]
    fn any_captured_session_clears_the_flag() {
        // One captured session proves the hook works even if local files differ
        // in count (resume reuse maps many local files to one server session).
        let local = vec![scan(AgentKind::ClaudeCode, 12, 4)];
        let rows = build_rows(&local, &captured(&[("claude-code", 3)]));
        assert!(!row(&rows, "claude-code").uncaptured);
    }

    #[test]
    fn only_historical_local_sessions_do_not_warn() {
        // Local sessions exist but none are recent → not actionable.
        let local = vec![scan(AgentKind::Codex, 5, 0)];
        let rows = build_rows(&local, &captured(&[]));
        assert!(!row(&rows, "codex").uncaptured);
    }

    #[test]
    fn kiro_v2_and_v3_local_counts_fold_under_one_agent_kind() {
        // Both Kiro engines map to AgentKind::KiroCli; their stores are
        // distinct but the server records one kind, so counts must sum.
        let local = vec![
            scan(AgentKind::KiroCli, 2, 1),
            scan(AgentKind::KiroCli, 3, 2),
        ];
        let rows = build_rows(&local, &captured(&[]));
        let kiro = row(&rows, "kiro-cli");
        assert_eq!(kiro.local_total, 5);
        assert_eq!(kiro.local_recent, 3);
        assert!(kiro.uncaptured);
    }

    #[test]
    fn captured_only_agent_is_shown_and_never_flagged() {
        // The server captured sessions for an agent we found no local store for
        // (e.g. it ran on another machine). Show it, but it is not a local gap.
        let rows = build_rows(&[], &captured(&[("gemini-cli", 4)]));
        let gemini = row(&rows, "gemini-cli");
        assert_eq!(gemini.local_total, 0);
        assert_eq!(gemini.captured, 4);
        assert!(!gemini.uncaptured);
    }

    #[test]
    fn agents_with_neither_local_nor_captured_are_dropped() {
        let rows = build_rows(&[scan(AgentKind::Codex, 0, 0)], &captured(&[]));
        assert!(rows.is_empty(), "empty rows should not be listed: {rows:?}");
    }

    #[test]
    fn uncaptured_rows_sort_first() {
        let local = vec![
            scan(AgentKind::ClaudeCode, 3, 1),
            scan(AgentKind::KimiCode, 2, 2),
        ];
        let rows = build_rows(&local, &captured(&[("claude-code", 3)]));
        assert_eq!(
            rows[0].agent, "kimi-code",
            "the warning must lead: {rows:?}"
        );
        assert!(rows[0].uncaptured);
    }

    #[test]
    fn recent_cutoff_zero_means_everything_recent() {
        assert!(recent_cutoff(SystemTime::now(), 0).is_none());
        assert!(is_recent(SystemTime::UNIX_EPOCH, None));
    }

    #[test]
    fn recent_cutoff_excludes_old_sessions() {
        let now = SystemTime::now();
        let cutoff = recent_cutoff(now, 30);
        let old = now - Duration::from_secs(31 * 86_400);
        let fresh = now - Duration::from_secs(86_400);
        assert!(!is_recent(old, cutoff));
        assert!(is_recent(fresh, cutoff));
    }

    /// End-to-end proof that `scan_local` wires to the real workstream path
    /// encoder: a Claude Code transcript planted under the harness's actual
    /// `~/.claude/projects/<cwd-with-slashes-as-dashes>/` layout, with a header
    /// naming this cwd, must be detected as one local ClaudeCode session — and
    /// a foreign-cwd transcript must be ignored. This is the detector guard the
    /// pure verdict tests cannot give, since the encoding lives in the
    /// workstream crate.
    ///
    /// Unix-gated: the fixture plants a POSIX-encoded `~/.claude/projects/<cwd>`
    /// path, and cross-platform native-store path handling is owned and tested by
    /// `ai-memory-workstream`. The pure aggregation tests above run everywhere.
    #[cfg(unix)]
    #[tokio::test]
    async fn scan_local_detects_a_planted_claude_session_for_this_cwd() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let other_cwd = tempfile::tempdir().unwrap();

        let projects = home.path().join(".claude").join("projects");
        let encoded = cwd.path().to_string_lossy().replace('/', "-");
        let session_dir = projects.join(&encoded);
        std::fs::create_dir_all(&session_dir).unwrap();
        let header = serde_json::json!({
            "sessionId": "11111111-2222-3333-4444-555555555555",
            "cwd": cwd.path().to_string_lossy(),
        });
        std::fs::write(session_dir.join("sess.jsonl"), format!("{header}\n")).unwrap();

        // A transcript for a different cwd, planted under this cwd's encoded
        // directory, must not be miscounted — the header's cwd wins.
        let foreign = serde_json::json!({
            "sessionId": "99999999-8888-7777-6666-555555555555",
            "cwd": other_cwd.path().to_string_lossy(),
        });
        std::fs::write(session_dir.join("foreign.jsonl"), format!("{foreign}\n")).unwrap();

        let scans = scan_local(home.path(), cwd.path(), 0).await;
        let claude = scans
            .iter()
            .find(|s| s.agent == AgentKind::ClaudeCode)
            .unwrap_or_else(|| panic!("expected a ClaudeCode scan; got {scans:?}"));
        assert_eq!(claude.total, 1, "only the matching-cwd session counts");
        assert_eq!(claude.recent, 1, "since_days 0 makes it recent");

        // And a project with no local stores yields no scans at all.
        let empty_home = tempfile::tempdir().unwrap();
        let none = scan_local(empty_home.path(), cwd.path(), 0).await;
        assert!(none.is_empty(), "no stores should mean no scans: {none:?}");
    }
}
