//! `askama` template definitions and per-route view-models.

use askama::Template;

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

fn encode_path(path: &str) -> String {
    path.split('/')
        .map(encode_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_segment(segment: &str) -> String {
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

/// Format an ISO-8601 timestamp string as a relative human-readable string
/// (e.g. "3 hours ago", "2 days ago"). Falls back to the raw string on any
/// parse error.
#[must_use]
pub(crate) fn humanize(iso: &str) -> String {
    let Ok(then) = iso.parse::<jiff::Timestamp>() else {
        return iso.to_owned();
    };
    let now = jiff::Timestamp::now();
    // Compute elapsed seconds using microsecond arithmetic to avoid Span API.
    let diff_us = now.as_microsecond() - then.as_microsecond();
    let secs = diff_us.abs() / 1_000_000;
    if secs < 60 {
        return "just now".to_owned();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins} minute{} ago", if mins == 1 { "" } else { "s" });
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours} hour{} ago", if hours == 1 { "" } else { "s" });
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{days} day{} ago", if days == 1 { "" } else { "s" });
    }
    let months = days / 30;
    if months < 12 {
        return format!("{months} month{} ago", if months == 1 { "" } else { "s" });
    }
    let years = months / 12;
    format!("{years} year{} ago", if years == 1 { "" } else { "s" })
}

// ---------------------------------------------------------------------------
// projects.html
// ---------------------------------------------------------------------------

/// One card on the project-list page.
pub(crate) struct ProjectCard {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Number of latest pages.
    pub page_count: u64,
    /// Humanised timestamp (e.g. "3 hours ago"), or empty string.
    pub last_updated_relative: String,
    /// Link target (`w/{ws}/{proj}`, relative to `<base href>`).
    pub href: String,
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
    /// All project cards, sorted by most recently active first.
    pub projects: Vec<ProjectCard>,
    /// Present whenever a migration receipt exists (dialog dismissal is
    /// client-side, per browser).
    pub okf_dialog: Option<OkfDialog>,
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
}

/// A folder in the sidebar tree (groups pages by first path segment).
pub(crate) struct Folder {
    /// Folder name (first path segment, without trailing slash).
    pub name: String,
    /// Pages inside this folder.
    pub pages: Vec<PageRow>,
}

/// View-model for `GET /w/:workspace/:project`.
#[derive(Template)]
#[template(path = "project.html")]
pub(crate) struct ProjectView {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Sidebar folder tree — knowledge pages only.
    pub folders: Vec<Folder>,
    /// Machinery pages (lint reports, sessions, logs, indexes),
    /// rendered collapsed below the knowledge tree.
    pub system: Vec<Folder>,
    /// N most-recent knowledge pages for the right column.
    pub recent: Vec<PageRow>,
}

/// View-model for a namespace (directory) listing — `GET
/// /w/:workspace/:project/p/:namespace/` when the path names a namespace
/// rather than a page (#603).
#[derive(Template)]
#[template(path = "namespace.html")]
pub(crate) struct NamespaceView {
    /// Workspace name.
    pub workspace: String,
    /// Project name.
    pub project: String,
    /// Link back to the project overview.
    pub project_href: String,
    /// The namespace path (no trailing slash), e.g. `_lint` or `concepts`.
    pub namespace: String,
    /// Pages under this namespace.
    pub pages: Vec<PageRow>,
}

// ---------------------------------------------------------------------------
// page.html
// ---------------------------------------------------------------------------

/// View-model for `GET /w/:workspace/:project/p/*path`.
#[derive(Template)]
#[template(path = "page.html")]
pub(crate) struct PageView {
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
    /// Humanised updated timestamp.
    pub updated_relative: String,
    /// Humanised created timestamp.
    pub created_relative: String,
    /// Path of the page this supersedes, or empty string.
    pub supersedes_path: String,
    /// Link target for the superseded page, or empty string.
    pub supersedes_href: String,
    /// Rendered markdown body (HTML, trusted).
    pub body_html: String,
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
// search.html
// ---------------------------------------------------------------------------

/// One FTS5 search hit.
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
}

/// View-model for `GET /search?q=…`.
#[derive(Template)]
#[template(path = "search.html")]
pub(crate) struct SearchView {
    /// The raw query string.
    pub query: String,
    /// FTS5 search hits.
    pub hits: Vec<SearchHit>,
    /// Pre-computed hit count for display (avoids needing `|length` filter).
    pub hit_count: usize,
}

// ---------------------------------------------------------------------------
// not_found.html
// ---------------------------------------------------------------------------

/// View-model for 404 responses.
#[derive(Template)]
#[template(path = "not_found.html")]
pub(crate) struct NotFoundView {}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
