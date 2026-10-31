//! `askama` template definitions and per-route view-models.

use askama::Template;

/// Server version shown in the footer of every page. Workspace crates share
/// one version, so this crate's version is the running server's.
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

/// Build a project URL with path segments percent-encoded.
///
/// The URL is **relative** (`w/{ws}/{proj}`, no leading slash) so it
/// resolves against the page's injected `<base href>` — which the server
/// sets to `{base_path}{web_slug}/`. That keeps every link correct whether
/// the browser is served at the host root (`/web/…`) or under a reverse-proxy
/// subpath (`/wiki/web/…`), without the templates knowing the prefix.
#[must_use]
pub(crate) fn project_href(workspace: &str, project: &str) -> String {
    format!(
        "w/{}/{}",
        encode_segment(workspace),
        encode_segment(project)
    )
}

/// Build a `/web` page URL with workspace/project/path percent-encoded.
#[must_use]
pub(crate) fn page_href(workspace: &str, project: &str, path: &str) -> String {
    format!(
        "{}/p/{}",
        project_href(workspace, project),
        encode_path(path)
    )
}

/// Build a page-history URL (`w/{ws}/{proj}/h/{path}`), percent-encoded.
#[must_use]
pub(crate) fn history_href(workspace: &str, project: &str, path: &str) -> String {
    format!(
        "{}/h/{}",
        project_href(workspace, project),
        encode_path(path)
    )
}

/// Build a session-timeline URL (`w/{ws}/{proj}/s/{id}`), percent-encoded.
#[must_use]
pub(crate) fn session_href(workspace: &str, project: &str, session_id: &str) -> String {
    format!(
        "{}/s/{}",
        project_href(workspace, project),
        encode_segment(session_id)
    )
}

/// Build a project link-graph URL (`w/{ws}/{proj}/graph`), percent-encoded.
#[must_use]
pub(crate) fn graph_href(workspace: &str, project: &str) -> String {
    format!("{}/graph", project_href(workspace, project))
}

/// The last eight characters of a UUID, for display. The ids here are
/// UUIDv7, whose leading characters are a shared timestamp prefix.
#[must_use]
pub(crate) fn short_id(id: &str) -> String {
    let skip = id.chars().count().saturating_sub(8);
    id.chars().skip(skip).collect()
}

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

/// Compact human-readable token count (`12345` → `"12.3k"`, `2_100_000` →
/// `"2.1M"`). Values under 1000 render as a plain integer.
#[must_use]
pub(crate) fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Human-readable label for an agent-kind badge (`claude-code` → `Claude
/// Code`). Purely a display concern — the wire form
/// ([`ai_memory_core::AgentKind::as_str`]) stays the identity used
/// everywhere else.
#[must_use]
pub(crate) fn agent_label(kind: ai_memory_core::AgentKind) -> &'static str {
    use ai_memory_core::AgentKind;
    match kind {
        AgentKind::ClaudeCode => "Claude Code",
        AgentKind::Codex => "Codex",
        AgentKind::OpenCode => "OpenCode",
        AgentKind::Cursor => "Cursor",
        AgentKind::GeminiCli => "Gemini CLI",
        AgentKind::ClaudeDesktop => "Claude Desktop",
        AgentKind::OpenClaw => "OpenClaw",
        AgentKind::AntigravityCli => "Antigravity CLI",
        AgentKind::Omp => "Oh My Pi",
        AgentKind::Pi => "Pi",
        AgentKind::Crush => "Crush",
        AgentKind::Grok => "Grok Build CLI",
        AgentKind::Zero => "Zero",
        AgentKind::Devin => "Devin",
        AgentKind::KimiCode => "Kimi Code",
        AgentKind::KiroCli => "Kiro CLI",
        AgentKind::CommandCode => "Command Code",
        AgentKind::Hermes => "Hermes",
        AgentKind::Pool => "Pool",
        AgentKind::Zcode => "ZCode",
        AgentKind::CopilotCli => "Copilot CLI",
        AgentKind::Grizzybot => "GrizzyBot",
        AgentKind::Droid => "Droid",
        AgentKind::Muse => "Muse",
        AgentKind::CodeBuddy => "CodeBuddy",
        AgentKind::IbmBob => "IBM Bob",
        AgentKind::Dsh => "DSH",
        AgentKind::Cline => "Cline",
        AgentKind::QwenCode => "Qwen Code",
        AgentKind::Qoder => "Qoder",
        AgentKind::Other => "Other",
    }
}

pub(crate) fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(&mut out, "%{byte:02X}");
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Humanise helper
// ---------------------------------------------------------------------------

/// Humanised session duration between two ISO-8601 timestamps, or `"open"`
/// when `ended` is absent. Falls back to `"—"` on any parse error rather
/// than a raw/misleading duration.
#[must_use]
pub(crate) fn duration_between(started: &str, ended: Option<&str>) -> String {
    let Some(ended) = ended else {
        return "open".to_owned();
    };
    let (Ok(start), Ok(end)) = (
        started.parse::<jiff::Timestamp>(),
        ended.parse::<jiff::Timestamp>(),
    ) else {
        return "—".to_owned();
    };
    let secs = (end.as_microsecond() - start.as_microsecond())
        .abs()
        .saturating_div(1_000_000);
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours < 24 {
        return if rem_mins == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {rem_mins}m")
        };
    }
    let days = hours / 24;
    format!("{days}d")
}

// ---------------------------------------------------------------------------
// projects.html
// ---------------------------------------------------------------------------

/// One card on the project-list page.
pub(crate) struct ProjectCard {
    pub agent_summary: String,
    pub inbox_href: String,
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Number of latest pages.
    pub page_count: u64,
    /// Captured sessions, whether or not they have produced a page yet.
    pub session_count: u64,
    /// Sanitized hook observations recorded for the project.
    pub observation_count: u64,
    /// Sessions that have not emitted SessionEnd yet.
    pub open_session_count: u64,
    /// Humanised timestamp (e.g. "3 hours ago"), or empty string.
    pub last_updated_relative: String,
    /// Messages from other projects waiting for an agent here; `0` hides the
    /// badge.
    pub pending_inbox: u64,
    /// Link target (`w/{ws}/{proj}`, relative to `<base href>`).
    pub href: String,
}

/// Aggregate metadata for the human-only dashboard. Prompt bodies, command
/// output, paths, and handoff prose intentionally do not appear here.
pub(crate) struct DashboardStats {
    pub project_count: usize,
    pub page_count: u64,
    pub session_count: u64,
    pub observation_count: u64,
    pub open_session_count: u64,
    pub last_activity_relative: String,
    /// Process-local hook capture health. Empty only for standalone embeds;
    /// the production server always supplies it.
    pub ingest: Option<IngestStats>,
}

/// Content-free snapshot of hook ingestion health since this server started.
pub(crate) struct IngestStats {
    pub accepted: u64,
    pub dropped_by_policy: u64,
    pub shed_saturated: u64,
    pub shed_rate_limited: u64,
}

/// One project named in the home page's pending-mail summary.
pub(crate) struct MailSummaryItem {
    /// `workspace/project`.
    pub label: String,
    /// Link target (`w/{ws}/{proj}`, relative to `<base href>`).
    pub href: String,
    /// Pending messages waiting there.
    pub pending: u64,
}

/// The one-time 2.0 migration explainer dialog (docs/okf.md): shown
/// whenever a migration receipt exists, dismissed per browser via a
/// "do not show me again" checkbox persisted in localStorage keyed by
/// the migration timestamp (a future migration re-shows it).
pub(crate) struct OkfDialog {
    /// Absolute archive path from the receipt.
    pub archive_path: String,
    /// Human-readable archive size.
    pub size_human: String,
    /// Migration timestamp — also the localStorage dismissal key.
    pub created_at: String,
    /// Whether the archive file still exists (adapts the recovery text).
    pub archive_present: bool,
}

/// View-model for `GET /`.
#[derive(Template)]
#[template(path = "projects.html")]
pub(crate) struct ProjectsView {
    pub i18n: ai_memory_core::i18n::Localizer,
    /// All project cards, sorted by most recently active first.
    pub projects: Vec<ProjectCard>,
    /// Privacy-preserving aggregate signals above the project cards.
    pub dashboard: DashboardStats,
    /// Initial project-card filter selected by a metric link.
    pub project_state: String,
    /// Present whenever a migration receipt exists (dialog dismissal is
    /// client-side, per browser).
    pub okf_dialog: Option<OkfDialog>,
    /// Pending messages across the projects listed in `projects`; `0` hides
    /// the summary strip.
    pub mail_total: u64,
    /// Projects the strip names, most mail first, capped.
    pub mail_projects: Vec<MailSummaryItem>,
    /// Projects with mail beyond the ones `mail_projects` names.
    pub mail_hidden: usize,
}

// ---------------------------------------------------------------------------
// project.html
// ---------------------------------------------------------------------------

/// One entry in the sidebar or recent-list.
pub(crate) struct PageRow {
    /// Relative wiki path.
    pub path: String,
    /// Link target for this page.
    pub href: String,
    /// Page title.
    pub title: String,
    /// Semantic kind badge text.
    pub kind: String,
    /// Humanised updated timestamp.
    pub updated_relative: String,
    /// Harness that most recently contributed evidence to this page
    /// version, if any is recorded (P2 evidence substrate). `None` for
    /// hand-written pages or writes that predate it — omitted from the
    /// badge row rather than shown as "unknown".
    pub agent_label: Option<&'static str>,
}

/// One agent-kind badge with a session count, for the project stats card.
pub(crate) struct AgentBadge {
    /// Human-readable label (`Claude Code`, `Codex`, ...).
    pub label: &'static str,
    /// Sessions recorded for this agent in the project.
    pub count: u64,
}

/// One pending inbox message on the project page.
pub(crate) struct MailboxRow {
    /// Subject line, or a placeholder when the sender gave none.
    pub subject: String,
    /// Epistemic classification supplied by the sender.
    pub nature: &'static str,
    /// `workspace/project` of the sender, or a neutral label when the viewer
    /// may not read it.
    pub sender: String,
    /// Link to the sender's project; `None` when the viewer may not read it.
    pub sender_href: Option<String>,
    /// Leading characters of the body. Untrusted cross-project text: the
    /// template escapes it and never renders it as markup.
    pub snippet: String,
    /// Humanised creation timestamp.
    pub created_relative: String,
}

/// A folder in the sidebar tree (groups pages by first path segment).
pub(crate) struct Folder {
    /// Folder name (first path segment, without trailing slash).
    pub name: String,
    /// Namespace listing for the folder; `None` for the `(root)` bucket,
    /// which has no namespace URL.
    pub href: Option<String>,
    /// Pages in the folder, all of them.
    pub total: usize,
    /// The most recently updated pages, capped; every page for `(root)`.
    pub pages: Vec<PageRow>,
}

/// View-model for `GET /w/:workspace/:project`.
#[derive(Template)]
#[template(path = "project.html")]
pub(crate) struct ProjectView {
    pub i18n: ai_memory_core::i18n::Localizer,
    pub agents: Vec<crate::operations::AgentCard>,
    pub inbox_href: String,
    pub project_href: String,
    pub token_usage: TokenUsagePanel,
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Capture metadata for the project. Deliberately excludes raw prompt,
    /// command and observation bodies from the human dashboard.
    pub stats: ProjectMemoryStats,
    /// Sidebar folder tree — knowledge pages only.
    pub folders: Vec<Folder>,
    /// Machinery pages (lint reports, sessions, logs, indexes),
    /// rendered collapsed below the knowledge tree.
    pub system: Vec<Folder>,
    /// N most-recent knowledge pages for the right column.
    pub recent: Vec<PageRow>,
    /// Sessions grouped by the agent CLI that ran them, most-used first.
    /// Empty when the project has no sessions yet.
    pub by_agent: Vec<AgentBadge>,
    /// Pending inbox messages, oldest first, capped at the route's limit.
    pub mailbox: Vec<MailboxRow>,
    /// All pending messages for the project; exceeds `mailbox.len()` when the
    /// list is capped.
    pub mailbox_total: u64,
    /// `mailbox` lists fewer messages than are pending.
    pub mailbox_capped: bool,
    /// The project's most recent sessions the viewer may see.
    pub sessions: Vec<SessionRow>,
    /// Link to this project's link graph.
    pub graph_href: String,
}

/// One recent session on the project page.
pub(crate) struct SessionRow {
    /// Human-readable harness label.
    pub agent_label: &'static str,
    /// Session duration or open marker.
    pub duration: String,
    /// Reported token usage or an em dash when unavailable.
    pub tokens: String,
    /// Link to the session timeline.
    pub href: String,
    /// Agent kind as stored (`claude-code`, `codex`, …).
    pub agent_kind: String,
    /// Last eight characters of the session id.
    pub short_id: String,
    /// Observations this session left in the project.
    pub observation_count: u64,
    /// Humanised start timestamp.
    pub started_relative: String,
    /// The session has not ended.
    pub open: bool,
}

/// Privacy-preserving project-level capture summary.
pub(crate) struct ProjectMemoryStats {
    /// Number of compiled memory pages.
    pub page_count: u64,
    /// Sessions recorded for this project.
    pub session_count: u64,
    /// Sanitized events recorded for the project.
    pub observation_count: u64,
    /// Sessions without a SessionEnd event.
    pub open_session_count: u64,
    /// Humanised latest capture timestamp, if one exists.
    pub last_activity_relative: String,
    /// Compact total input+output tokens across every session with a
    /// reported usage, or "—" when none has reported any yet.
    pub tokens_total: String,
}

// ---------------------------------------------------------------------------
// workstreams.html
// ---------------------------------------------------------------------------

/// One managed ledger on the root/operator operations page.
pub(crate) struct WorkstreamRow {
    /// Link to the workstream's project overview.
    pub project_href: String,
    /// Unambiguous `workspace/project` display label.
    pub scope_label: String,
    /// Human-readable managed-workstream name.
    pub name: String,
    /// Last eight characters of the stable workstream id.
    pub short_id: String,
    /// Stable lifecycle status token used by template conditionals.
    pub status: String,
    /// Human-facing lifecycle status.
    pub status_label: String,
    /// Harness used by the latest managed run, or empty before first launch.
    pub latest_agent: String,
    /// Last eight characters of the latest managed run id, or empty.
    pub latest_run_short: String,
    /// Current harness links, in recency order.
    pub linked_harnesses: Vec<String>,
    /// Portable events imported across all runs.
    pub event_count: u64,
    /// Humanised last selection/import time.
    pub last_active_relative: String,
    /// Humanised latest run start, or empty.
    pub latest_started_relative: String,
    /// Humanised latest run end, or empty.
    pub latest_ended_relative: String,
}

/// Root/operator managed-workstream overview.
#[derive(Template)]
#[template(path = "workstreams.html")]
pub(crate) struct WorkstreamsView {
    pub i18n: ai_memory_core::i18n::Localizer,
    /// Current page rows, attention-first then newest activity.
    pub rows: Vec<WorkstreamRow>,
    /// One-based page number.
    pub page: usize,
    /// Previous page link, when applicable.
    pub prev_href: Option<String>,
    /// Next page link, when another row exists.
    pub next_href: Option<String>,
}

/// View-model for a namespace (directory) listing — `GET
/// /w/:workspace/:project/p/:namespace/` when the path names a namespace
/// rather than a page (#603).
#[derive(Template)]
#[template(path = "namespace.html")]
pub(crate) struct NamespaceView {
    pub i18n: ai_memory_core::i18n::Localizer,
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Link back to the project overview.
    pub project_href: String,
    /// The namespace path (no trailing slash), e.g. `_lint` or `concepts`.
    pub namespace: String,
    /// The current page of pages under this namespace.
    pub pages: Vec<PageRow>,
    /// Pages in the whole namespace.
    pub total: usize,
    /// 1-based position of the first listed page.
    pub first: usize,
    /// 1-based position of the last listed page.
    pub last: usize,
    /// Sorting by title instead of recency.
    pub sort_title: bool,
    /// Namespace listing sorted by recency.
    pub sort_updated_href: String,
    /// Namespace listing sorted by title.
    pub sort_title_href: String,
    /// Link to the previous page of results, if any.
    pub prev_href: Option<String>,
    /// Link to the next page of results, if any.
    pub next_href: Option<String>,
}

// ---------------------------------------------------------------------------
// page.html
// ---------------------------------------------------------------------------

/// How many backlinks or outgoing links the page sidebar lists per group.
pub(crate) const RELATED_LIMIT: usize = 10;

/// One entry of a page's "On this page" outline.
pub(crate) struct OutlineItem {
    /// Link to the heading, anchored on the page's own URL.
    pub href: String,
    /// Heading text.
    pub text: String,
    /// Whether the heading is an h3 (indented under its h2).
    pub nested: bool,
}

/// One row of a page's "Cited by" or "Cites" group.
pub(crate) struct RelatedRow {
    /// Link to the related page.
    pub href: String,
    /// Title of the related page.
    pub title: String,
    /// `workspace/project` when the page lives in another project, else empty.
    pub scope_hint: String,
}

/// View-model for `GET /w/:workspace/:project/p/*path`.
#[derive(Template)]
#[template(path = "page.html")]
pub(crate) struct PageView {
    pub i18n: ai_memory_core::i18n::Localizer,
    pub knowledge: crate::knowledge::KnowledgeView,
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Link target for the containing project.
    pub project_href: String,
    /// Relative wiki path.
    pub path: String,
    /// Page title.
    pub title: String,
    /// Semantic kind.
    pub kind: String,
    /// Memory tier.
    pub tier: String,
    /// Whether the page is pinned.
    pub pinned: bool,
    /// Whether the page's `expires_at` TTL has passed. Search and the
    /// listings already leave such a page out; its own URL still opens it,
    /// so the view says so.
    pub expired: bool,
    /// Humanised updated timestamp.
    pub updated_relative: String,
    /// Humanised created timestamp.
    pub created_relative: String,
    /// Exact updated timestamp (ISO 8601), shown on hover.
    pub updated_at: String,
    /// Exact created timestamp (ISO 8601), shown on hover.
    pub created_at: String,
    /// Path of the page this supersedes, or empty string.
    pub supersedes_path: String,
    /// Link target for the superseded page, or empty string.
    pub supersedes_href: String,
    /// Rendered markdown body (HTML, trusted).
    pub body_html: String,
    /// Link to this page's version history.
    pub history_href: String,
    /// h2/h3 headings of the body, empty with fewer than two.
    pub outline: Vec<OutlineItem>,
    /// Pages that link to this one, at most [`RELATED_LIMIT`].
    pub cited_by: Vec<RelatedRow>,
    /// Backlinks beyond [`RELATED_LIMIT`].
    pub cited_by_more: usize,
    /// Pages this one links to, at most [`RELATED_LIMIT`].
    pub cites: Vec<RelatedRow>,
    /// Outgoing links beyond [`RELATED_LIMIT`].
    pub cites_more: usize,
    /// Username of the page's last author. Empty string when the page
    /// was authored anonymously / by root / pre-multi-user — the
    /// template uses the empty check to omit the "Last edited by"
    /// chip entirely, so legacy pages render with the exact same
    /// chrome they had before v0.8.
    pub author_username: String,
    /// Optional display name shown in parens after the username
    /// (e.g. `alice (Alice Smith)`). Empty when not set on the user row.
    pub author_name: String,
    /// Optional email rendered as a `mailto:` link after the username.
    /// Empty when not set on the user row.
    pub author_email: String,
}

// ---------------------------------------------------------------------------
// history.html
// ---------------------------------------------------------------------------

/// One stored version in the history list.
pub(crate) struct HistoryVersionRow {
    /// Full version id, submitted by the compare form.
    pub id: String,
    /// Last eight characters of the id.
    pub short_id: String,
    /// Short id of the version this one replaced, or empty.
    pub supersedes_short: String,
    /// The live version of the path.
    pub is_latest: bool,
    /// Title stored on this version.
    pub title: String,
    /// ISO-8601 write instant.
    pub created_at: String,
    /// Humanised write instant.
    pub created_relative: String,
    /// Username of an attributed write, or empty.
    pub author: String,
    /// Body length in characters.
    pub body_chars: u64,
    /// Selected as the older side of the diff.
    pub is_from: bool,
    /// Selected as the newer side of the diff.
    pub is_to: bool,
    /// Link diffing this version against the one it replaced, or empty.
    pub compare_href: String,
}

/// One diff line; `text` is untrusted page content and is escaped.
pub(crate) struct HistoryDiffLine {
    /// `+`, `-` or a space.
    pub sign: String,
    /// The line itself.
    pub text: String,
}

/// One diff hunk.
pub(crate) struct HistoryHunk {
    /// `@@ -a,b +c,d @@`.
    pub header: String,
    /// The hunk's lines.
    pub lines: Vec<HistoryDiffLine>,
}

/// View-model for `GET /w/:workspace/:project/h/*path`.
#[derive(Template)]
#[template(path = "history.html")]
pub(crate) struct HistoryView {
    pub i18n: ai_memory_core::i18n::Localizer,
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Link to the project.
    pub project_href: String,
    /// Link to the live page.
    pub page_href: String,
    /// This route, without a query string (the compare form's action).
    pub history_href: String,
    /// Wiki path.
    pub path: String,
    /// Every version stored at the path.
    pub total: u64,
    /// 1-based position of the first listed version (0 when none).
    pub shown_start: usize,
    /// 1-based position of the last listed version.
    pub shown_end: usize,
    /// Versions per listing page.
    pub limit: usize,
    /// Link to newer versions, or empty.
    pub newer_href: String,
    /// Link to older versions, or empty.
    pub older_href: String,
    /// The listed versions, newest first.
    pub versions: Vec<HistoryVersionRow>,
    /// Short id of the older side, or empty when the diff starts from nothing.
    pub from_short: String,
    /// Write instant of the older side, or empty.
    pub from_created: String,
    /// Title of the older side, or empty.
    pub from_title: String,
    /// Short id of the newer side.
    pub to_short: String,
    /// Write instant of the newer side.
    pub to_created: String,
    /// Title of the newer side.
    pub to_title: String,
    /// The two bodies are identical.
    pub same: bool,
    /// Why the diff was refused, or empty.
    pub too_large: String,
    /// The output line cap cut the diff short.
    pub truncated: bool,
    /// Diff hunks.
    pub hunks: Vec<HistoryHunk>,
}

// ---------------------------------------------------------------------------
// session.html
// ---------------------------------------------------------------------------

/// One observation on the session timeline; every text field is untrusted
/// captured text and is escaped by the template.
pub(crate) struct SessionObservationRow {
    /// 1-based position in capture order within this project.
    pub position: usize,
    /// Observation kind.
    pub kind: String,
    /// Observation title.
    pub title: String,
    /// Body, capped like the API's default `body_max_chars`.
    pub body: String,
    /// ISO-8601 capture instant.
    pub created_at: String,
    /// Humanised capture instant.
    pub created_relative: String,
}

/// View-model for `GET /w/:workspace/:project/s/:session_id`.
#[derive(Template)]
#[template(path = "session.html")]
pub(crate) struct SessionView {
    pub i18n: ai_memory_core::i18n::Localizer,
    pub token_usage: TokenUsagePanel,
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Link to the project.
    pub project_href: String,
    /// This route, without a query string (the jump form's action).
    pub session_href: String,
    /// Session id.
    pub session_id: String,
    /// Agent kind as stored.
    pub agent_kind: String,
    /// ISO-8601 start.
    pub started_at: String,
    /// ISO-8601 end, or empty while open.
    pub ended_at: String,
    /// Observations in this project.
    pub total: u64,
    /// Observations per page.
    pub limit: usize,
    /// 1-based position of the first shown observation (0 when none).
    pub shown_start: usize,
    /// 1-based position of the last shown observation.
    pub shown_end: usize,
    /// Rows the session left in other projects (not shown here).
    pub elided_other_scope: u64,
    /// Link to the first page, or empty on it.
    pub first_href: String,
    /// Link to the previous page, or empty.
    pub prev_href: String,
    /// Link to the next page, or empty.
    pub next_href: String,
    /// Link to the last page, or empty on it.
    pub last_href: String,
    /// Prefilled value of the jump field.
    pub jump_value: usize,
    /// This page's observations.
    pub observations: Vec<SessionObservationRow>,
}

// ---------------------------------------------------------------------------
// search.html
// ---------------------------------------------------------------------------

/// One search hit.
pub(crate) struct SearchHit {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Relative wiki path.
    pub path: String,
    /// Link target for this hit.
    pub href: String,
    /// Page title.
    pub title: String,
    /// FTS5 snippet (HTML-marked with `<mark>` tags).
    pub snippet: String,
    /// How the ranker placed this hit; set on an explained project search.
    pub trace: Option<HitTrace>,
}

/// One hit's recall trace: what each stream saw and how the fused score
/// became the final order. Every number is pre-formatted for display.
pub(crate) struct HitTrace {
    /// 1-based position in the result list.
    pub position: usize,
    /// The returned rank (lower is better).
    pub rank: String,
    /// Sum of the RRF contributions.
    pub fused: String,
    /// The bounded authority multiplier, or `—` when the page had none.
    pub authority: String,
    /// The query-intent boost, when routing applied one.
    pub intent_boost: Option<String>,
    /// Reranker relevance, when the reranker judged this hit.
    pub rerank_score: Option<String>,
    /// One row per stream that ran.
    pub streams: Vec<StreamTraceRow>,
    /// Entity names that matched the query, comma-separated.
    pub matched_entities: String,
    /// `seed path (direction[, edge])` when the graph stream pulled it in.
    pub graph_via: Option<String>,
}

/// One stream's view of one hit.
pub(crate) struct StreamTraceRow {
    /// Stream name as `streams_active` reports it.
    pub stream: &'static str,
    /// 1-based rank in that stream, or `—` when the hit missed it.
    pub rank: String,
    /// The stream's raw score (bm25, cosine, entity weight), or `—`.
    pub raw: String,
    /// The stream's RRF contribution.
    pub rrf: String,
}

/// One stream's wall time on an explained search.
pub(crate) struct TimingRow {
    /// `fts`, `entity`, `vector`, `graph`, `observation` or `rerank`.
    pub stream: &'static str,
    /// Milliseconds, formatted.
    pub ms: String,
}

/// One option of the search page's kind or tier filter.
pub(crate) struct FilterOption {
    /// The value the select submits and shows.
    pub value: String,
    /// The current search filters on this value.
    pub selected: bool,
}

/// One project in the search page's "Search in" selector.
pub(crate) struct ScopeOption {
    /// `workspace/project`, the value the selector submits.
    pub value: String,
    /// Project name shown under its workspace's group.
    pub label: String,
    /// The current search is scoped to this project.
    pub selected: bool,
}

/// The projects of one workspace in the "Search in" selector.
pub(crate) struct ScopeGroup {
    /// Workspace name, the group label.
    pub workspace: String,
    /// Readable projects of the workspace.
    pub projects: Vec<ScopeOption>,
}

/// View-model for `GET /search?q=…`.
#[derive(Template)]
#[template(path = "search.html")]
pub(crate) struct SearchView {
    pub i18n: ai_memory_core::i18n::Localizer,
    /// The raw query string.
    pub query: String,
    /// Search hits, best first.
    pub hits: Vec<SearchHit>,
    /// Pre-computed hit count for display (avoids needing `|length` filter).
    pub hit_count: usize,
    /// Workspace of a project search; empty for a cross-project search.
    pub workspace: String,
    /// Project of a project search; empty for a cross-project search.
    pub project: String,
    /// The viewer asked for the recall trace.
    pub explain: bool,
    /// Streams that ran, comma-separated (explain only).
    pub streams_active: String,
    /// Per-stream wall time (explain only).
    pub timings: Vec<TimingRow>,
    /// `provider / model (dim N)` of the embedder, or `none`.
    pub embedder: String,
    /// `name / model` of the reranker, or `none`.
    pub reranker: String,
    /// More matches existed than `max_results`; the list is cut.
    pub truncated: bool,
    /// Most rows the page renders.
    pub max_results: usize,
    /// The same query across every project.
    pub global_href: String,
    /// Projects the viewer may read, grouped by workspace.
    pub scope_groups: Vec<ScopeGroup>,
    /// Link to the scoped project, for the breadcrumb.
    pub project_href: String,
    /// A kind or tier filter narrowed the hits.
    pub filtered: bool,
    /// The kind filter names no standard kind (empty when it does, or when
    /// there is none); a custom frontmatter kind or a typo.
    pub unknown_kind: String,
    /// Kinds the filter offers.
    pub kind_options: Vec<FilterOption>,
    /// Tiers the filter offers.
    pub tier_options: Vec<FilterOption>,
}

impl SearchView {
    /// A project search (hybrid ranker) rather than a cross-project one.
    pub(crate) fn scoped(&self) -> bool {
        !self.workspace.is_empty()
    }
}

// ---------------------------------------------------------------------------
// pending.html / admin_required.html
// ---------------------------------------------------------------------------

/// One pending auto-improvement proposal on the triage page.
pub(crate) struct PendingRow {
    /// Proposal id; the page posts it to `/admin/pending-writes/{id}/…`.
    pub id: String,
    /// Workspace name, sent as the `workspace` query parameter.
    pub workspace: String,
    /// Project name, sent as the `project` query parameter.
    pub project: String,
    /// `workspace/project`, the filter value and display label.
    pub scope_label: String,
    /// Proposal category.
    pub kind: String,
    /// `create` or `update`.
    pub operation: String,
    /// Target wiki path.
    pub target_path: String,
    /// Link to the current page, for proposals that rewrite one.
    pub target_href: String,
    /// Proposal title.
    pub title: String,
    /// Reviewer confidence as a whole percentage.
    pub confidence_pct: i64,
    /// Humanised stage time.
    pub staged_relative: String,
    /// `full_page` or `patch`.
    pub edit_mode: String,
    /// Why the reviewer proposed this edit.
    pub rationale: String,
    /// Full proposed page body, shown as plain text.
    pub body_markdown: String,
    /// The target is under `_rules/`, which every agent session loads.
    pub is_rule: bool,
    /// The proposal rewrites an existing page.
    pub rewrites_existing: bool,
    /// `ai-memory pending-writes approve` for this row, quoted for a POSIX
    /// shell and shown to a browser that can only read the page.
    pub posix_cli_approve: String,
    /// The matching POSIX-shell `reject` command.
    pub posix_cli_reject: String,
    /// The matching command quoted for PowerShell.
    pub powershell_cli_approve: String,
    /// The matching PowerShell `reject` command.
    pub powershell_cli_reject: String,
}

/// One `<option>` in a triage-page filter.
pub(crate) struct SelectOption {
    /// Submitted value.
    pub value: String,
    /// Display text.
    pub label: String,
    /// Currently selected.
    pub selected: bool,
}

/// View-model for `GET /pending`.
#[derive(Template)]
#[template(path = "pending.html")]
pub(crate) struct PendingView {
    pub i18n: ai_memory_core::i18n::Localizer,
    /// Proposals after the project filter, in the chosen order.
    pub rows: Vec<PendingRow>,
    /// Project filter options, the "all projects" option first.
    pub projects: Vec<SelectOption>,
    /// Sort options.
    pub sorts: Vec<SelectOption>,
    /// Pending proposals across every project, before the filter.
    pub total: u64,
    /// Distinct projects with a pending proposal.
    pub project_count: usize,
    /// The selection held more pending proposals than the page reads.
    pub truncated: bool,
    /// The read cap, shown when `truncated`.
    pub limit: usize,
    /// The browser signed in with the legacy Basic credential, which admits
    /// GET only: the decision buttons give way to the CLI commands.
    pub read_only: bool,
}

// ---------------------------------------------------------------------------
// health.html
// ---------------------------------------------------------------------------

/// One failed health check.
pub(crate) struct HealthCheckRow {
    /// Stable check id.
    pub id: &'static str,
    /// `warn` or `error`.
    pub status: &'static str,
    /// What is wrong.
    pub detail: String,
    /// The exact command that fixes it, when there is one.
    pub fix: Option<String>,
}

/// One label/value line on the health page.
pub(crate) struct HealthRow {
    /// Label.
    pub label: String,
    /// Value.
    pub value: String,
}

/// A titled group of health rows.
pub(crate) struct HealthSection {
    /// Section heading.
    pub title: String,
    /// Its rows.
    pub rows: Vec<HealthRow>,
}

/// One node in the server-rendered SVG link graph.
pub(crate) struct GraphSvgNode {
    /// Target for the node (`w/…` or `w/…/p/…`).
    pub href: String,
    /// Full label for the SVG tooltip.
    pub tooltip: String,
    /// Truncated label drawn next to the node.
    pub label: String,
    /// Circle centre x, in viewBox pixels.
    pub x: i32,
    /// Circle centre y, in viewBox pixels.
    pub y: i32,
    /// Baseline y of the label.
    pub label_y: i32,
}

/// One SVG line between two graph nodes.
pub(crate) struct GraphSvgEdge {
    /// Start x.
    pub x1: i32,
    /// Start y.
    pub y1: i32,
    /// End x.
    pub x2: i32,
    /// End y.
    pub y2: i32,
}

/// One row of the graph's accessible edge table.
pub(crate) struct GraphEdgeRow {
    /// Link to the source page.
    pub from_href: String,
    /// Source label (path, or `workspace/project:path`).
    pub from_label: String,
    /// Link to the target page.
    pub to_href: String,
    /// Target label.
    pub to_label: String,
}

/// View-model for `GET /graph` and `GET /w/:workspace/:project/graph`.
#[derive(Template)]
#[template(path = "graph.html")]
pub(crate) struct GraphView {
    pub i18n: ai_memory_core::i18n::Localizer,
    /// Project graph (scoped header search and breadcrumb) vs the global one.
    pub is_project: bool,
    /// Workspace name; empty on the global graph.
    pub workspace: String,
    /// Project name; empty on the global graph.
    pub project: String,
    /// Link back to the project overview; empty on the global graph.
    pub project_href: String,
    /// Page heading.
    pub heading: String,
    /// One-line explanation under the heading.
    pub blurb: String,
    /// Empty-state copy when there are no nodes.
    pub empty_message: String,
    /// The node cap dropped some pages.
    pub truncated: bool,
    /// Nodes actually drawn.
    pub shown: usize,
    /// Cap shown in the truncation notice.
    pub node_cap: usize,
    /// SVG nodes.
    pub svg_nodes: Vec<GraphSvgNode>,
    /// SVG edges.
    pub svg_edges: Vec<GraphSvgEdge>,
    /// Accessible edge table.
    pub rows: Vec<GraphEdgeRow>,
}

/// View-model for the root-only `GET /health`.
#[derive(Template)]
#[template(path = "health.html")]
pub(crate) struct HealthView {
    pub i18n: ai_memory_core::i18n::Localizer,
    /// The host wired the process-side sources (hook counters, spool…).
    pub wired: bool,
    /// Failed checks, most severe first.
    pub checks: Vec<HealthCheckRow>,
    /// Counter groups.
    pub sections: Vec<HealthSection>,
}

/// View-model for a non-root session that opens a root-only page.
#[derive(Template)]
#[template(path = "admin_required.html")]
pub(crate) struct AdminRequiredView {
    pub i18n: ai_memory_core::i18n::Localizer,
}

// ---------------------------------------------------------------------------
// not_found.html
// ---------------------------------------------------------------------------

/// View-model for 404 responses.
#[derive(Template)]
#[template(path = "not_found.html")]
pub(crate) struct NotFoundView {
    pub i18n: ai_memory_core::i18n::Localizer,
}

// ---------------------------------------------------------------------------
// login.html / change_password.html
// ---------------------------------------------------------------------------

/// View-model for `GET /login` (public builtin auth page).
#[derive(Template)]
#[template(path = "login.html")]
pub(crate) struct LoginView {
    pub i18n: ai_memory_core::i18n::Localizer,
    /// Sanitised post-login redirect target (absolute path under the wiki root).
    pub next: String,
    /// Absolute path to the change-password page (for `must_change_password`).
    pub change_password_href: String,
}

/// View-model for `GET /change-password` (public builtin auth page).
#[derive(Template)]
#[template(path = "change_password.html")]
pub(crate) struct ChangePasswordView {
    pub i18n: ai_memory_core::i18n::Localizer,
    /// Sanitised post-change redirect target (absolute path under the wiki root).
    pub next: String,
}
/// Metadata-only operational view of measured responses.
pub(crate) struct TokenUsagePanel {
    pub model: String,
    pub session: String,
    pub from: String,
    pub until: String,
    pub turn: String,
    pub tool: String,
    pub agents: Vec<UsageAgentOption>,
    pub matching_tokens: String,
    pub search: String,
    pub recent: bool,
    pub signals_only: bool,
    pub matching: u64,
    pub shown_start: u64,
    pub shown_end: u64,
    pub previous_href: String,
    pub next_href: String,
    pub observation_offset: usize,
    pub observation_limit: usize,
    pub count: u64,
    pub input: String,
    pub output: String,
    pub cache_write: String,
    pub cache_read: String,
    pub hit_rate: String,
    pub rows: Vec<TokenUsageRow>,
}
pub(crate) struct TokenUsageRow {
    pub turn: String,
    pub turn_href: String,
    pub response_native: String,
    pub session: String,
    pub recorded_at: String,
    pub time_basis: &'static str,
    pub tools: Vec<UsageTool>,
    pub total_value: u64,
    pub observed_at: String,
    pub href: String,
    pub id: String,
    pub model: String,
    pub agent: String,
    pub input: String,
    pub output: String,
    pub cache: String,
    pub total: String,
    pub when: String,
    pub signal: String,
}

pub(crate) struct UsageAgentOption {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

pub(crate) struct UsageTool {
    pub id: String,
    pub name: String,
    pub relation: &'static str,
    pub is_error: Option<bool>,
    pub href: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_auth_pages_do_not_show_the_server_version() {
        let i18n = ai_memory_core::i18n::Localizer::default();
        let version = format!("v{VERSION}");
        let login = LoginView {
            i18n: i18n.clone(),
            next: "/web/".into(),
            change_password_href: "/web/change-password".into(),
        }
        .render()
        .unwrap();
        let change = ChangePasswordView {
            i18n: i18n.clone(),
            next: "/web/".into(),
        }
        .render()
        .unwrap();
        let signed_in = NotFoundView { i18n: i18n.clone() }.render().unwrap();
        assert!(!login.contains(&version), "login page leaks {version}");
        assert!(
            !change.contains(&version),
            "change-password page leaks {version}"
        );
        assert!(
            signed_in.contains(&version),
            "signed-in pages show {version}"
        );
    }

    #[test]
    fn href_helpers_percent_encode_segments() {
        // Relative (no leading slash) so they resolve against the injected
        // `<base href>` — see `project_href` docs.
        assert_eq!(
            project_href("default space", "proj#one"),
            "w/default%20space/proj%23one"
        );
        assert_eq!(
            page_href("default", "scratch", "notes/a b%25.md"),
            "w/default/scratch/p/notes/a%20b%2525.md"
        );
        assert_eq!(
            graph_href("default space", "proj#one"),
            "w/default%20space/proj%23one/graph"
        );
    }
}
