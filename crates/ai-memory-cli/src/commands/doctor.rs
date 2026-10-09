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
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use ai_memory_core::AgentKind;
use ai_memory_hooks::CaptureDisposition;
use ai_memory_workstream::{
    ManagedHarness, build_launch_plan, list_native_sessions, native_memory_dir,
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
    ManagedHarness::Copilot,
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
    captured: Option<u64>,
    /// True when the harness ran here recently but captured nothing — the
    /// high-confidence "hook is missing" signal.
    uncaptured: Option<bool>,
    mixed_capture_sessions: Option<u64>,
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
    #[serde(default)]
    mixed_capture_sessions: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct ProjectCoordinateReport {
    local: super::hook_capture::RepositoryCoordinateEvidence,
    server: Option<ai_memory_store::ProjectCoordinateDiagnostic>,
    server_problem: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CaptureCoverageProblem {
    reason: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct OperatorIdentity {
    version: String,
    level: String,
    operator: Option<String>,
    distinguishes_operators: bool,
}

async fn read_identity(ep: &ServerEndpoint) -> Option<OperatorIdentity> {
    match get_json::<OperatorIdentity>(ep, "/identity", &[]).await {
        Ok(identity) => Some(identity),
        Err(error) if super::is_scope_not_found(&error) => None,
        Err(_) => {
            eprintln!("Machine identity is unavailable; showing capture coverage only.");
            None
        }
    }
}

/// The full report, also the JSON output shape.
#[derive(Debug, Serialize)]
struct DoctorReport {
    workspace: String,
    project: String,
    server: String,
    since_days: u32,
    rows: Vec<CoverageRow>,
    capture_coverage_problem: Option<CaptureCoverageProblem>,
    /// The agent kinds (kebab form) flagged as uncaptured, for quick scripting.
    uncaptured: Vec<String>,
    /// Set when the nearest `.ai-memory.toml`'s `[capture]` section is
    /// `PolicyState::Invalid`. This fails CLOSED (every file/shell tool event
    /// is reduced to metadata until the marker is fixed; nothing leaks), but
    /// with no other signal anywhere that the marker stopped working as
    /// configured, so `doctor` is the one place that surfaces it.
    marker_capture_problem: Option<MarkerCaptureProblem>,
    capture_owner_active: bool,
    identity: Option<OperatorIdentity>,
    project_coordinate: Option<ProjectCoordinateReport>,
    /// Native "memory" stores found for harnesses that keep one, and whether
    /// the nearest marker would exclude a read of them (harness-issue
    /// #1003). Only Claude Code's location is known today; harnesses with no
    /// established convention are simply absent from this list.
    native_memory: Vec<NativeMemoryReport>,
}

#[derive(Debug, Clone, Serialize)]
struct MarkerCaptureProblem {
    classification: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct NativeMemoryReport {
    agent: String,
    path: String,
    file_count: usize,
    /// One of `"excluded"`, `"not excluded"`, `"marker invalid"`, or
    /// `"metadata only"` (an active, valid policy whose extraction for this
    /// specific probe fell back to the metadata-only disposition — kept
    /// distinct from `"not excluded"` rather than folded into it, since the
    /// two mean different things: the dispositions are mapped one-to-one,
    /// never collapsed).
    status: String,
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
                captured: Some(captured),
                uncaptured: Some(uncaptured),
                mixed_capture_sessions: None,
            }
        })
        .filter(|row| row.local_total > 0 || row.captured.is_some_and(|count| count > 0))
        .collect();

    // Surface the actionable warnings first, then a stable alphabetical order.
    rows.sort_by(|a, b| {
        b.uncaptured
            .unwrap_or(false)
            .cmp(&a.uncaptured.unwrap_or(false))
            .then_with(|| a.agent.cmp(&b.agent))
    });
    rows
}

/// Enumerate local native sessions for `cwd` across every scanned harness.
/// Read-only. A harness whose store is unreadable, absent, or unsupported
/// simply contributes nothing — the command never invents a gap it cannot see.
pub(crate) async fn scan_local(home: &Path, cwd: &Path, since_days: u32) -> Vec<LocalScan> {
    scan_local_with(home, cwd, since_days, relocated_session_dir).await
}

/// Where `harness` keeps its sessions when the environment relocates its home
/// (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `KIMI_CODE_HOME`, …), via the same
/// launch-plan resolver `ai-memory run` uses. `None` means the default
/// `$HOME`-relative store.
pub(crate) fn relocated_session_dir(harness: ManagedHarness) -> Option<PathBuf> {
    build_launch_plan(harness, None, Vec::new(), None)
        .ok()
        .and_then(|plan| plan.session_dir)
}

/// Harnesses whose native "memory" store location is known, so `doctor` can
/// report on it (harness-issue #1003). Only Claude Code's is established
/// today — `~/.claude/projects/<project>/memory/`, found via
/// [`native_memory_dir`]'s content-based match rather than a guessed,
/// platform-specific directory name. A harness left out of this list simply
/// produces no report line, which is the documented behavior for "location
/// unknown" rather than an error.
const HARNESSES_WITH_KNOWN_NATIVE_MEMORY: &[ManagedHarness] = &[ManagedHarness::Claude];

/// For every harness in [`HARNESSES_WITH_KNOWN_NATIVE_MEMORY`] that actually
/// has a memory store for this checkout, report its location, how many
/// files it holds, and whether the nearest marker would exclude a read of
/// it.
///
/// `cwd` is resolved to the repository root first (worktrees and
/// subdirectories collapse onto it): Claude Code keys its auto-memory by
/// repository root, so [`native_memory_dir`] -- which only matches a session
/// recorded for the exact cwd it is given -- would otherwise find nothing
/// when `doctor` runs from a subdirectory or a linked worktree.
///
/// The exclusion check reuses two already-verified read-only facts rather
/// than re-deriving anything: [`super::hook_capture::capture_config_problem`]
/// (a broken marker is reported as `"marker invalid"`, the same diagnosis
/// `doctor`'s other check makes) and, when the marker parses and compiles,
/// [`ai_memory_hooks::CapturePolicy::inspect`] on a synthetic read of one
/// file inside the store — the exact evaluation the native hook performs on
/// a real `Read` tool call, just never sent anywhere. Every
/// `CaptureDisposition` maps to its own distinct status string; none are
/// folded together.
fn native_memory_reports(home: &Path, cwd: &Path) -> Vec<NativeMemoryReport> {
    let repo_root =
        ai_memory_consolidate::discover_main_repo_root(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let Some(repo_root_str) = repo_root.to_str() else {
        return Vec::new();
    };
    let mut reports = Vec::new();
    for &harness in HARNESSES_WITH_KNOWN_NATIVE_MEMORY {
        let session_dir = relocated_session_dir(harness);
        let Some(memory_dir) = native_memory_dir(harness, home, &repo_root, session_dir.as_deref())
        else {
            continue;
        };
        let file_count = std::fs::read_dir(&memory_dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|ft| ft.is_file()))
                    .count()
            })
            .unwrap_or(0);
        let status = if super::hook_capture::capture_config_problem(repo_root_str).is_some() {
            "marker invalid"
        } else {
            let policy = super::hook_capture::capture_policy(repo_root_str);
            let probe_path = memory_dir.join("probe.md");
            let raw = serde_json::json!({
                "tool_name": "Read",
                "tool_input": { "file_path": probe_path.to_string_lossy() },
            });
            let decision = policy.inspect(harness.agent_kind(), &raw, repo_root_str);
            match decision.protocol().disposition() {
                CaptureDisposition::Drop => "excluded",
                CaptureDisposition::Keep => "not excluded",
                CaptureDisposition::MetadataOnly => "metadata only",
            }
        };
        reports.push(NativeMemoryReport {
            agent: harness.agent_kind().as_str().to_owned(),
            path: memory_dir.display().to_string(),
            file_count,
            status: status.to_owned(),
        });
    }
    reports
}

/// [`scan_local`] with the relocation lookup passed in. The lookup reads the
/// process environment, so a test that plants a fixture under a temporary
/// `$HOME` passes `|_| None`: otherwise a developer's `CLAUDE_CONFIG_DIR`
/// wins over that `$HOME` and the fixture is never found.
async fn scan_local_with(
    home: &Path,
    cwd: &Path,
    since_days: u32,
    session_dir_for: impl Fn(ManagedHarness) -> Option<PathBuf>,
) -> Vec<LocalScan> {
    let recent_cutoff = recent_cutoff(SystemTime::now(), since_days);
    let mut scans = Vec::new();
    for &harness in SCANNED_HARNESSES {
        // Honor harness home relocations (see `relocated_session_dir`); fall
        // back to the default $HOME-relative store when there is none.
        let session_dir = session_dir_for(harness);
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

async fn project_coordinate_report(
    ep: &ServerEndpoint,
    identity_cwd: &str,
    marker: &crate::marker::MarkerInspection,
    workspace: &str,
    project: &str,
    explicit_workspace: Option<&str>,
    explicit_project: Option<&str>,
) -> ProjectCoordinateReport {
    let inspection = super::hook_capture::inspect_repository_coordinate(
        identity_cwd,
        marker,
        explicit_workspace,
        explicit_project,
    );
    let mut query = vec![("workspace", workspace), ("project", project)];
    if let Some(repository) = inspection.repository.as_ref() {
        query.push(("identity", repository.identity.as_str()));
        query.push(("identity_source", repository.source.as_str()));
        if let Some(style) = inspection.evidence.identity_style.as_deref() {
            query.push(("identity_style", style));
        }
    }
    match get_json::<ai_memory_store::ProjectCoordinateDiagnostic>(
        ep,
        "/admin/project-coordinate",
        &query,
    )
    .await
    {
        Ok(server) => ProjectCoordinateReport {
            local: inspection.evidence,
            server: Some(server),
            server_problem: None,
        },
        Err(error) => ProjectCoordinateReport {
            local: inspection.evidence,
            server: None,
            server_problem: Some(ai_memory_core::Sanitizer::builtin().scrub(&error.to_string())),
        },
    }
}

/// Run the capture-coverage doctor.
///
/// # Errors
/// Returns an error when the current scope cannot be resolved, the working
/// directory is unreadable, or the server cannot be reached / its response
/// cannot be parsed.
pub async fn run(config: &Config, args: crate::cli::DoctorArgs) -> Result<()> {
    let (identity_cwd, lookup_cwd) = super::scope_directories(config)
        .ok_or_else(|| anyhow::anyhow!("resolving the current working directory"))?;
    let explicit_scope = args
        .workspace
        .as_deref()
        .filter(|value| !value.is_empty())
        .is_some()
        && args
            .project
            .as_deref()
            .filter(|value| !value.is_empty())
            .is_some();
    let marker = if explicit_scope {
        crate::marker::MarkerInspection {
            status: "bypassed_explicit_scope",
            fields: crate::marker::RoutingFields::default(),
            scope: None,
            route_identity: None,
        }
    } else {
        let marker =
            crate::marker::inspect_scope_for(&lookup_cwd, &identity_cwd, &config.runtime_env);
        if marker.status == "invalid_home_routes" {
            anyhow::bail!("invalid home route map; doctor did not query a fallback server scope");
        }
        marker
    };
    let scope_marker = marker
        .scope
        .clone()
        .map(|scope| (scope, identity_cwd.clone(), lookup_cwd.clone()));
    let (workspace, project) = super::resolve_scope_with_marker(
        config,
        args.workspace.as_deref(),
        args.project.as_deref(),
        scope_marker,
        false,
    )?;

    let cwd = PathBuf::from(&identity_cwd);
    let home =
        super::run::native_home(config).context("locating the local harness session stores")?;

    let ep = ServerEndpoint::from_config_resolving_auth(config).await;
    let project_coordinate = Some(
        project_coordinate_report(
            &ep,
            &identity_cwd,
            &marker,
            &workspace,
            &project,
            args.workspace.as_deref(),
            args.project.as_deref(),
        )
        .await,
    );
    let mut capture_coverage_problem = None;
    let captured = match get_json::<ByAgentResponse>(
        &ep,
        "/admin/sessions/by-agent",
        &[
            ("workspace", workspace.as_str()),
            ("project", project.as_str()),
        ],
    )
    .await
    {
        Ok(response) => response.by_agent,
        Err(error) if super::is_scope_not_found(&error) => Vec::new(),
        Err(error) if super::is_scope_ambiguous(&error) => {
            capture_coverage_problem = Some(CaptureCoverageProblem {
                reason: "server scope is ambiguous; captured counts are unavailable".to_owned(),
            });
            Vec::new()
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("asking the server for captured session counts for {workspace}/{project}")
            });
        }
    };

    let identity = read_identity(&ep).await;
    let local = scan_local(&home, &cwd, args.since_days).await;

    let counts = captured
        .iter()
        .map(|count| (count.agent.clone(), count.sessions))
        .collect();
    let mut rows = build_rows(&local, &counts);
    for row in &mut rows {
        if capture_coverage_problem.is_some() {
            row.captured = None;
            row.uncaptured = None;
        }
        row.mixed_capture_sessions = captured
            .iter()
            .find(|count| count.agent == row.agent)
            .and_then(|count| count.mixed_capture_sessions);
    }
    let uncaptured: Vec<String> = rows
        .iter()
        .filter(|r| r.uncaptured == Some(true))
        .map(|r| r.agent.clone())
        .collect();

    let marker_capture_problem = cwd.to_str().and_then(|cwd| {
        super::hook_capture::capture_config_problem(cwd).map(|_| MarkerCaptureProblem {
            classification: "settings_marker_invalid",
        })
    });

    let native_memory = native_memory_reports(&home, &cwd);

    let report = DoctorReport {
        workspace,
        project,
        server: ep.url.clone(),
        since_days: args.since_days,
        rows,
        capture_coverage_problem,
        uncaptured,
        marker_capture_problem,
        capture_owner_active: config.runtime_env.capture_owner_active(),
        identity,
        project_coordinate,
        native_memory,
    };

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        render_human(&report);
    }
    Ok(())
}

fn render_project_coordinate(coordinate: &ProjectCoordinateReport) -> String {
    use std::fmt::Write as _;
    let mut output = String::new();
    match &coordinate.server {
        Some(server) => {
            let current = server.current_name.as_deref().unwrap_or("none");
            let _ = writeln!(
                output,
                "  project coordinate: {} (requested {}, current {})",
                server.status, server.requested_name, current
            );
            if server.rename_eligible {
                output.push_str("    an authorized write can promote this project name in place\n");
            }
            if let Some(reason) = &server.collision_reason {
                let _ = writeln!(output, "    collision: {reason}");
            }
        }
        None => {
            let _ = writeln!(
                output,
                "  project coordinate: unavailable ({})",
                coordinate
                    .server_problem
                    .as_deref()
                    .unwrap_or("unknown error")
            );
        }
    }
    let local = &coordinate.local;
    let _ = writeln!(
        output,
        "    local: scope {}, marker {}",
        local.scope_source, local.marker_status
    );
    if let Some(source) = &local.identity_source {
        let _ = writeln!(
            output,
            "    repository: source {source}, style {}",
            local
                .identity_style
                .as_deref()
                .unwrap_or(ai_memory_core::repository_identity::IdentityStyle::Path.as_str())
        );
    }
    if let Some(canonical) = &local.canonical_candidate {
        let _ = writeln!(output, "    canonical candidate: {canonical}");
    }
    if let Some(legacy) = &local.legacy_candidate {
        let _ = writeln!(output, "    legacy candidate: {legacy}");
    }
    for message in &local.messages {
        let _ = writeln!(output, "    marker diagnostic: {message}");
    }
    output
}

fn render_human(report: &DoctorReport) {
    println!(
        "Capture coverage for {}/{} (server {})",
        report.workspace, report.project, report.server
    );
    if let Some(identity) = &report.identity {
        println!(
            "  identity: {} ({}, server {})",
            identity.operator.as_deref().unwrap_or("anonymous"),
            identity.level,
            identity.version
        );
    } else {
        println!("  identity: unavailable");
    }
    if report.capture_owner_active {
        println!(
            "  AI_MEMORY_CAPTURE_OWNER is active: native capture is suppressed in this process.\n  \
             Confirm the external producer is delivering events."
        );
    }
    if let Some(coordinate) = &report.project_coordinate {
        print!("{}", render_project_coordinate(coordinate));
    }
    if let Some(problem) = &report.capture_coverage_problem {
        println!("  capture coverage: unavailable ({})", problem.reason);
    }
    if report.since_days == 0 {
        println!("  recent window: all on-disk sessions\n");
    } else {
        println!("  recent window: last {} days\n", report.since_days);
    }

    if let Some(problem) = &report.marker_capture_problem {
        println!(
            "⚠ {}: the settings marker has an invalid `[capture]` section.\n  \
             It fails closed — file and shell tool content is reduced to metadata there, \
             nothing leaks — but its `ignore_paths` exclusions are NOT applying while it stays \
             invalid. Fix the TOML and re-run `ai-memory doctor` to confirm.\n",
            problem.classification
        );
    }

    if !report.native_memory.is_empty() {
        println!(
            "Note: a shell/PowerShell hook install (the Docker-wrapper default) does not \
             enforce capture-policy v1 — \"excluded\" below only holds on a native hook \
             install or a generated integration. Check the configured hook command; a dry-run \
             `ai-memory install-hooks` reports the selected install path, not the active one.\n"
        );
    }

    if report.rows.is_empty() {
        if report.capture_coverage_problem.is_some() {
            println!(
                "  No local harness sessions found for this project; server capture counts are unavailable."
            );
        } else {
            println!(
                "  No local harness sessions found for this project and nothing captured yet."
            );
        }
        return;
    }

    for row in &report.rows {
        let mark = match row.uncaptured {
            Some(true) => "⚠",
            Some(false) => "✓",
            None => "?",
        };
        let captured = row
            .captured
            .map_or_else(|| "unavailable".to_owned(), |count| count.to_string());
        println!(
            "  {mark} {:<14} {:>4} local ({} recent) → {:>11} captured",
            row.agent, row.local_total, row.local_recent, captured
        );
        if row.uncaptured == Some(true) {
            if report.capture_owner_active {
                println!(
                    "      No captured session: check the external producer and repository policy."
                );
            } else {
                println!(
                    "      └ ran here but nothing was captured — install its hook:\n        \
                     ai-memory install-hooks --agent {} --apply",
                    row.agent
                );
            }
        }
        if let Some(count) = row.mixed_capture_sessions.filter(|count| *count > 0) {
            println!(
                "      {count} session(s) contain events from multiple capture sources.\n      \
                 Backfill can also mix source metadata; this does not prove duplicate capture."
            );
        }
        if let Some(memory) = report.native_memory.iter().find(|m| m.agent == row.agent) {
            println!(
                "      native memory: {} ({} file{})",
                memory.path,
                memory.file_count,
                if memory.file_count == 1 { "" } else { "s" }
            );
            println!("      capture: {}", memory.status);
        }
    }

    if report.capture_coverage_problem.is_some() {
        println!("\nCapture verdict unavailable: server capture counts could not be resolved.");
    } else if report.uncaptured.is_empty() {
        println!("\n✓ Every harness that ran in this project recently is captured on the server.");
    } else {
        let remedy = if report.capture_owner_active {
            "Check the external producer and repository policy, then re-run `ai-memory doctor`."
        } else {
            "Install the hook(s) above, then re-run `ai-memory doctor` to confirm."
        };
        println!(
            "\n⚠ {} harness(es) ran here recently with no captured sessions: {}.\n  {remedy}",
            report.uncaptured.len(),
            report.uncaptured.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_coordinate(
        status: ai_memory_store::ProjectCoordinateStatus,
        collision_reason: Option<ai_memory_store::ProjectCoordinateCollisionReason>,
    ) -> ai_memory_store::ProjectCoordinateDiagnostic {
        ai_memory_store::ProjectCoordinateDiagnostic {
            workspace: "default".into(),
            requested_name: "acme-api".into(),
            status,
            project_id: Some(ai_memory_core::ProjectId::new()),
            current_name: Some("api".into()),
            canonical_candidate: Some("acme-api".into()),
            legacy_candidate: Some("api".into()),
            identity_source: Some("git_remote".into()),
            identity_style: Some("path".into()),
            rename_eligible: collision_reason.is_none(),
            collision_reason,
            candidate_count: 1,
        }
    }

    #[test]
    fn project_coordinate_renders_promotion_and_cross_forge_fallback_concisely() {
        let local = super::super::hook_capture::RepositoryCoordinateEvidence {
            marker_status: "valid",
            scope_source: "marker_or_fallback",
            identity_source: Some("git_remote".into()),
            identity_style: Some("path".into()),
            canonical_candidate: Some("acme-api".into()),
            legacy_candidate: Some("api".into()),
            messages: Vec::new(),
        };
        let promotion = render_project_coordinate(&ProjectCoordinateReport {
            local: local.clone(),
            server: Some(server_coordinate(
                ai_memory_store::ProjectCoordinateStatus::CanonicalCompat,
                None,
            )),
            server_problem: None,
        });
        assert!(promotion.contains("canonical_compat"), "{promotion}");
        assert!(
            promotion.contains("promote this project name in place"),
            "{promotion}"
        );

        let collision = render_project_coordinate(&ProjectCoordinateReport {
            local,
            server: Some(server_coordinate(
                ai_memory_store::ProjectCoordinateStatus::Ambiguous,
                Some(ai_memory_store::ProjectCoordinateCollisionReason::CrossForgeCollision),
            )),
            server_problem: None,
        });
        assert!(collision.contains("cross_forge_collision"), "{collision}");
        assert!(
            !collision.contains("promote this project name"),
            "{collision}"
        );
    }

    #[test]
    fn local_coordinate_reports_marker_precedence_invalid_toml_and_no_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let nested = repo.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(&repo)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .args(["-C"])
            .arg(&repo)
            .args([
                "remote",
                "add",
                "origin",
                "https://user:top-secret@example.test/acme/api.git",
            ])
            .status()
            .unwrap();
        std::fs::write(
            repo.join(".ai-memory.toml"),
            "workspace = \"marker-ws\"\nproject = \"marker-project\"\nidentity_style = \"path\"\n",
        )
        .unwrap();
        let marker = crate::marker::inspect_scope(
            &nested.to_string_lossy(),
            &crate::config::RuntimeEnv::default(),
        );
        let evidence = super::super::hook_capture::inspect_repository_coordinate(
            &nested.to_string_lossy(),
            &marker,
            Some("flag-ws"),
            Some("flag-project"),
        )
        .evidence;
        assert_eq!(evidence.marker_status, "valid");
        assert_eq!(evidence.scope_source, "cli");
        assert_eq!(evidence.identity_source.as_deref(), Some("git_remote"));
        assert_eq!(evidence.identity_style.as_deref(), Some("path"));
        assert!(evidence.messages.is_empty());
        let json = serde_json::to_string(&evidence).unwrap();
        assert!(!json.contains("top-secret"), "{json}");
        assert!(!json.contains("user@"), "{json}");

        std::fs::write(repo.join(".ai-memory.toml"), "identity_style = \"path\"\n").unwrap();
        let marker = crate::marker::inspect_scope(
            &nested.to_string_lossy(),
            &crate::config::RuntimeEnv::default(),
        );
        let remote = super::super::hook_capture::inspect_repository_coordinate(
            &nested.to_string_lossy(),
            &marker,
            Some("default"),
            Some("api"),
        )
        .evidence;
        assert_eq!(remote.identity_source.as_deref(), Some("git_remote"));
        assert_eq!(remote.identity_style.as_deref(), Some("path"));
        assert_eq!(remote.canonical_candidate.as_deref(), Some("acme-api"));
        let json = serde_json::to_string(&remote).unwrap();
        assert!(!json.contains("top-secret"), "{json}");
        assert!(!json.contains("user@"), "{json}");

        std::fs::write(
            repo.join(".ai-memory.toml"),
            "workspace = [\nsecret = \"top-secret\"\n",
        )
        .unwrap();
        let marker = crate::marker::inspect_scope(
            &nested.to_string_lossy(),
            &crate::config::RuntimeEnv::default(),
        );
        let invalid = super::super::hook_capture::inspect_repository_coordinate(
            &nested.to_string_lossy(),
            &marker,
            Some("default"),
            Some("repo"),
        )
        .evidence;
        assert_eq!(invalid.marker_status, "invalid");
        assert_eq!(
            invalid.messages,
            vec!["invalid marker TOML; marker routing was ignored"]
        );
        assert!(
            !serde_json::to_string(&invalid)
                .unwrap()
                .contains("top-secret")
        );
    }

    #[test]
    fn coordinate_server_errors_are_scrubbed_before_output() {
        let error = crate::http_client::server_response_error_for_test(
            reqwest::Method::GET,
            "/admin/project-coordinate",
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "Bearer abcdefghijklmnopqrstuvwxyz".into(),
        );
        let problem = ai_memory_core::Sanitizer::builtin().scrub(&error.to_string());
        assert!(!problem.contains("abcdefghijklmnopqrstuvwxyz"), "{problem}");
        assert!(problem.contains("[REDACTED:bearer_token]"), "{problem}");
    }

    #[tokio::test]
    async fn identity_diagnostics_degrade_on_server_and_decode_errors() {
        for (status, body, available) in [
            (500, "unavailable", false),
            (403, "forbidden", false),
            (200, "<html>legacy SPA</html>", false),
            (404, "missing", false),
            (
                200,
                r#"{"version":"2.5.0","level":"anonymous","operator":null,"distinguishes_operators":false}"#,
                true,
            ),
        ] {
            let app = axum::Router::new().route(
                "/identity",
                axum::routing::get(move || async move {
                    (axum::http::StatusCode::from_u16(status).unwrap(), body)
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let ep = ServerEndpoint::from_pair(
                Some(format!("http://{}", listener.local_addr().unwrap())),
                None,
            );
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            assert_eq!(
                read_identity(&ep).await.is_some(),
                available,
                "status {status}"
            );
            task.abort();
        }
    }

    #[test]
    fn an_older_server_leaves_provenance_unknown() {
        let legacy: ByAgentResponse =
            serde_json::from_str(r#"{"by_agent":[{"agent":"codex","sessions":2}]}"#).unwrap();
        assert_eq!(legacy.by_agent[0].sessions, 2);
        assert_eq!(legacy.by_agent[0].mixed_capture_sessions, None);
        let current: ByAgentResponse = serde_json::from_str(
            r#"{"by_agent":[{"agent":"codex","sessions":2,"mixed_capture_sessions":1}]}"#,
        )
        .unwrap();
        assert_eq!(current.by_agent[0].mixed_capture_sessions, Some(1));
    }

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
        assert_eq!(
            kimi.uncaptured,
            Some(true),
            "recent local + zero captured must warn"
        );
        assert_eq!(kimi.local_recent, 2);
        assert_eq!(kimi.captured, Some(0));
    }

    #[test]
    fn any_captured_session_clears_the_flag() {
        // One captured session proves the hook works even if local files differ
        // in count (resume reuse maps many local files to one server session).
        let local = vec![scan(AgentKind::ClaudeCode, 12, 4)];
        let rows = build_rows(&local, &captured(&[("claude-code", 3)]));
        assert_eq!(row(&rows, "claude-code").uncaptured, Some(false));
    }

    #[test]
    fn only_historical_local_sessions_do_not_warn() {
        // Local sessions exist but none are recent → not actionable.
        let local = vec![scan(AgentKind::Codex, 5, 0)];
        let rows = build_rows(&local, &captured(&[]));
        assert_eq!(row(&rows, "codex").uncaptured, Some(false));
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
        assert_eq!(kiro.uncaptured, Some(true));
    }

    #[test]
    fn captured_only_agent_is_shown_and_never_flagged() {
        // The server captured sessions for an agent we found no local store for
        // (e.g. it ran on another machine). Show it, but it is not a local gap.
        let rows = build_rows(&[], &captured(&[("gemini-cli", 4)]));
        let gemini = row(&rows, "gemini-cli");
        assert_eq!(gemini.local_total, 0);
        assert_eq!(gemini.captured, Some(4));
        assert_eq!(gemini.uncaptured, Some(false));
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
        assert_eq!(rows[0].uncaptured, Some(true));
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

        let scans = scan_local_with(home.path(), cwd.path(), 0, |_| None).await;
        let claude = scans
            .iter()
            .find(|s| s.agent == AgentKind::ClaudeCode)
            .unwrap_or_else(|| panic!("expected a ClaudeCode scan; got {scans:?}"));
        assert_eq!(claude.total, 1, "only the matching-cwd session counts");
        assert_eq!(claude.recent, 1, "since_days 0 makes it recent");

        // And a project with no local stores yields no scans at all.
        let empty_home = tempfile::tempdir().unwrap();
        let none = scan_local_with(empty_home.path(), cwd.path(), 0, |_| None).await;
        assert!(none.is_empty(), "no stores should mean no scans: {none:?}");
    }

    /// Plants a Claude Code session (for `cwd`) with a sibling `memory/`
    /// directory under `home`, the fixture every `native_memory_reports`
    /// test below starts from.
    fn claude_memory_fixture(home: &Path, cwd: &Path) -> PathBuf {
        let project_dir = home.join(".claude/projects/fixture");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("session.jsonl"),
            format!(
                "{}\n",
                serde_json::json!({
                    "sessionId": "11111111-2222-3333-4444-555555555555",
                    "cwd": cwd.to_string_lossy(),
                })
            ),
        )
        .unwrap();
        let memory_dir = project_dir.join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();
        memory_dir
    }

    fn find_claude_report(reports: &[NativeMemoryReport]) -> &NativeMemoryReport {
        reports
            .iter()
            .find(|r| r.agent == "claude-code")
            .unwrap_or_else(|| panic!("expected a claude-code report; got {reports:?}"))
    }

    #[test]
    fn native_memory_reports_is_not_excluded_without_a_marker() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let memory_dir = claude_memory_fixture(home.path(), cwd.path());
        std::fs::write(memory_dir.join("fact.md"), "x").unwrap();

        let reports = native_memory_reports(home.path(), cwd.path());
        let claude = find_claude_report(&reports);
        assert_eq!(claude.status, "not excluded");
        assert_eq!(claude.file_count, 1);
    }

    #[test]
    fn native_memory_reports_is_excluded_when_the_marker_covers_it() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let memory_dir = claude_memory_fixture(home.path(), cwd.path());
        std::fs::write(memory_dir.join("fact.md"), "x").unwrap();
        // An absolute pattern naming the fixture's own (fake) home, so this
        // does not depend on the real process `$HOME` the way a `~/...`
        // pattern would inside capture_policy's own home resolution.
        std::fs::write(
            cwd.path().join(".ai-memory.toml"),
            format!(
                "[capture]\nignore_paths = [{:?}]\n",
                format!("{}/.claude/projects/**", home.path().display())
            ),
        )
        .unwrap();

        let reports = native_memory_reports(home.path(), cwd.path());
        let claude = find_claude_report(&reports);
        assert_eq!(claude.status, "excluded");
    }

    #[test]
    fn marker_capture_problem_exposes_only_a_safe_classification() {
        let cwd = tempfile::tempdir().unwrap();
        let private_path = cwd.path().join("secret-project-name");
        std::fs::create_dir_all(&private_path).unwrap();
        std::fs::write(
            private_path.join(".ai-memory.toml"),
            "[capture] private-toml-sentinel\nignore_paths = [\"**\"]\n",
        )
        .unwrap();

        let problem =
            super::super::hook_capture::capture_config_problem(private_path.to_str().unwrap())
                .map(|_| MarkerCaptureProblem {
                    classification: "settings_marker_invalid",
                })
                .expect("invalid marker problem");
        let json = serde_json::to_string(&problem).unwrap();
        assert_eq!(json, r#"{"classification":"settings_marker_invalid"}"#);
        assert!(!json.contains(private_path.to_str().unwrap()));
        assert!(!json.contains("private-toml-sentinel"));
    }

    #[test]
    fn native_memory_reports_is_marker_invalid_for_a_broken_capture_table() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        claude_memory_fixture(home.path(), cwd.path());
        // The exact dropped-`#` shape found in practice: a stray token right
        // after a `[capture]` header.
        std::fs::write(
            cwd.path().join(".ai-memory.toml"),
            "[capture] this used to be a comment\nignore_paths = [\"**\"]\n",
        )
        .unwrap();

        let reports = native_memory_reports(home.path(), cwd.path());
        let claude = find_claude_report(&reports);
        assert_eq!(claude.status, "marker invalid");
    }

    #[test]
    fn native_memory_reports_file_count_ignores_subdirectories() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let memory_dir = claude_memory_fixture(home.path(), cwd.path());
        std::fs::write(memory_dir.join("fact.md"), "x").unwrap();
        std::fs::create_dir_all(memory_dir.join("a-subdirectory")).unwrap();
        std::fs::write(memory_dir.join("a-subdirectory/nested.md"), "y").unwrap();

        let reports = native_memory_reports(home.path(), cwd.path());
        let claude = find_claude_report(&reports);
        assert_eq!(
            claude.file_count, 1,
            "the subdirectory itself must not be counted as a file"
        );
    }

    /// The bug found in review: Claude Code keys its auto-memory by
    /// repository root, so running `doctor` from a subdirectory (or a linked
    /// worktree) must still resolve the root's `memory/` store, not come up
    /// empty because the session was recorded for the root and not the
    /// subdirectory `doctor` happened to run from.
    #[test]
    fn native_memory_reports_resolves_a_subdirectory_to_the_repository_root() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(repo.path())
            .status();
        if !matches!(status, Ok(s) if s.success()) {
            // No git binary in this environment: the repo-root resolution
            // falls back to the given cwd unchanged, so there is nothing
            // this test can distinguish here. Skip rather than fail.
            return;
        }
        let subdir = repo.path().join("sub/dir");
        std::fs::create_dir_all(&subdir).unwrap();
        // The session was recorded for the repository ROOT, as Claude Code
        // itself would record it -- not for the subdirectory.
        let memory_dir = claude_memory_fixture(home.path(), repo.path());
        std::fs::write(memory_dir.join("fact.md"), "x").unwrap();

        let reports = native_memory_reports(home.path(), &subdir);
        let claude = find_claude_report(&reports);
        assert_eq!(claude.file_count, 1);
    }
}
