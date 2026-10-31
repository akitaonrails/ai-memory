//! Shared pieces of the two agentmemory routes: the Obsidian vault
//! (`agentmemory-vault`) and the `ExportData` JSON file (`agentmemory-export`).
//!
//! Both plan every page before any request, write through the conditional
//! MCP tools (`create_only` for a page's first version, `expected_page_id` for
//! each later one), and never write under ai-memory's own reserved paths: the
//! destination is always below `--dest-prefix` (default
//! `imported/agentmemory`).
//!
//! Each page version ends with a provenance line naming its stable ingest key
//! (a UUIDv5 of the agentmemory record id). A rerun matches the complete body
//! against its planned versions before skipping what is there, so an
//! interrupted import resumes without duplicates and a finished one writes
//! nothing. A destination edit retaining the key is still a conflict.

pub(crate) mod export;
pub(crate) mod vault;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use uuid::Uuid;

use crate::ImportClient;
use crate::mcp::{McpWrite, Precondition, PreconditionFailed};

/// Where imported pages land unless `--dest-prefix` says otherwise.
pub(crate) const DEFAULT_DEST_PREFIX: &str = "imported/agentmemory";
/// Every page carries this tag so an import can be found and reviewed.
pub(crate) const IMPORT_TAG: &str = "agentmemory-import";
/// Bound on the tags one page carries.
const MAX_TAGS: usize = 16;
/// Bound on one tag, in characters.
const MAX_TAG_CHARS: usize = 64;
/// Bound on a derived title, in characters.
pub(crate) const MAX_TITLE_CHARS: usize = 80;
/// Bound on an explicit page title, in characters.
pub(crate) const MAX_EXPLICIT_TITLE_CHARS: usize = 200;
/// Bound on an agentmemory id, in bytes.
const MAX_ID_BYTES: usize = 256;

/// First path segments a destination prefix may never start with: ai-memory's
/// own session pages and maintained families. Any segment starting with `_`
/// or `.` (`_rules`, `_prompts`, `_global`, `.git`, ...) is refused as well.
const RESERVED_FIRST_SEGMENTS: &[&str] = &[
    "sessions",
    "session-logs",
    "procedures",
    "decisions",
    "gotchas",
    "profile",
];

/// The UUIDv5 namespace every agentmemory ingest key is derived in.
fn key_namespace() -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        b"https://github.com/akitaonrails/ai-memory/companions/ai-memory-importer/agentmemory",
    )
}

/// Stable ingest key for one agentmemory record: the same record always gets
/// the same key, whatever the destination or run.
pub(crate) fn ingest_key(record: &str, id: &str) -> String {
    Uuid::new_v5(&key_namespace(), format!("{record}:{id}").as_bytes()).to_string()
}

/// The attribution line that ends every imported page version. A rerun
/// compares the complete body, including this line, before skipping a version.
pub(crate) fn provenance_line(record: &str, id: &str, version: Option<u64>, key: &str) -> String {
    let version = version.map_or_else(String::new, |v| format!(", version {v}"));
    format!(
        "*Imported from agentmemory ({record} `{id}`{version}). {}*",
        key_marker(key)
    )
}

fn key_marker(key: &str) -> String {
    format!("Import key: `{key}`.")
}

/// Validate `--dest-prefix`: relative, portable, no traversal, and outside
/// every reserved family. Returns the normalized prefix (no trailing slash).
pub(crate) fn validate_dest_prefix(prefix: &str) -> Result<String> {
    let trimmed = prefix.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        bail!("--dest-prefix must not be empty");
    }
    if trimmed.starts_with('/') || trimmed.contains('\\') || trimmed.contains(':') {
        bail!("unsafe --dest-prefix {trimmed:?}: it must be a relative wiki path");
    }
    for segment in trimmed.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            bail!("unsafe --dest-prefix {trimmed:?}: empty, '.' or '..' segment");
        }
        if segment.starts_with('.')
            || !segment.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
            })
        {
            bail!(
                "unsafe --dest-prefix segment {segment:?}: use lowercase letters, digits, '-', '_' or '.'"
            );
        }
    }
    let first = trimmed.split('/').next().unwrap_or_default();
    if first.starts_with('_') || RESERVED_FIRST_SEGMENTS.contains(&first) {
        bail!("reserved --dest-prefix {first:?}: ai-memory owns that path family");
    }
    Ok(trimmed.to_owned())
}

/// Validate one planned destination path against the prefix it must stay
/// under, on top of the importer's general destination checks.
pub(crate) fn validate_planned_path(path: &str, prefix: &str) -> Result<()> {
    crate::validate_destination_path(path)?;
    let inside = path
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1);
    if !inside
        || path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == "..")
    {
        bail!("planned path {path:?} escapes the destination prefix {prefix:?}");
    }
    Ok(())
}

/// Validate an agentmemory record id before it names a path or a key.
pub(crate) fn validate_id(id: &str, what: &str) -> Result<()> {
    if id.trim().is_empty() || id.len() > MAX_ID_BYTES || id.chars().any(char::is_control) {
        bail!(
            "{what} id must be non-empty, at most {MAX_ID_BYTES} bytes and free of control characters"
        );
    }
    ai_memory_client::reject_sensitive(&serde_json::json!(id))
        .with_context(|| format!("{what} id looks like a credential; refusing it"))?;
    Ok(())
}

pub(crate) use ai_memory_client::display_session_id;

/// ai-memory `kind` and `tier` for an agentmemory memory `type`.
pub(crate) fn memory_kind_tier(memory_type: &str) -> (&'static str, &'static str) {
    match memory_type {
        "preference" => ("preference", "semantic"),
        "architecture" => ("decision", "semantic"),
        "workflow" => ("workflow", "procedural"),
        "pattern" => ("pattern", "procedural"),
        "bug" => ("bug", "semantic"),
        "fact" => ("fact", "semantic"),
        _ => ("memory", "semantic"),
    }
}

/// The first `max` characters of a one-line rendering of `text`.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(max).collect()
}

/// Sanitized, bounded, deduplicated tags, led by the import tag.
pub(crate) fn tags_from<'a>(raw: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut tags = vec![IMPORT_TAG.to_owned()];
    seen.insert(IMPORT_TAG.to_owned());
    for tag in raw {
        let tag = one_line(
            &ai_memory_client::sanitize_external_text(tag),
            MAX_TAG_CHARS,
        );
        if tag.is_empty() || !seen.insert(tag.clone()) {
            continue;
        }
        if tags.len() == MAX_TAGS {
            break;
        }
        tags.push(tag);
    }
    tags
}

/// Rewrite `[[target]]` and `[[target|alias]]` wikilinks. A target `resolve`
/// knows becomes a link to its imported path; any other target becomes plain
/// text (the alias when there is one), so no link points at a page the
/// import did not create.
pub(crate) fn rewrite_wikilinks(text: &str, resolve: impl Fn(&str) -> Option<String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &after[..end];
        if inner.contains('\n') || inner.contains("[[") {
            out.push_str("[[");
            rest = after;
            continue;
        }
        let (target, alias) = match inner.split_once('|') {
            Some((target, alias)) => (target.trim(), Some(alias.trim())),
            None => (inner.trim(), None),
        };
        match resolve(target) {
            Some(path) => {
                out.push_str("[[");
                out.push_str(&path);
                if let Some(alias) = alias.filter(|a| !a.is_empty()) {
                    out.push('|');
                    out.push_str(alias);
                }
                out.push_str("]]");
            }
            None => out.push_str(alias.filter(|a| !a.is_empty()).unwrap_or(target)),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// A wikilink target for an imported page: the path without `.md`.
pub(crate) fn link_target(path: &str) -> String {
    path.strip_suffix(".md").unwrap_or(path).to_owned()
}

/// Where a planned page goes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(crate) struct Destination {
    pub(crate) workspace: String,
    pub(crate) project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) project_id: Option<String>,
}

impl Destination {
    pub(crate) fn label(&self) -> &str {
        self.project_id.as_deref().unwrap_or(&self.project)
    }
}

/// One version of one page, already sanitized and carrying its provenance.
#[derive(Debug, Clone)]
pub(crate) struct PlannedVersion {
    pub(crate) key: String,
    pub(crate) source_id: String,
    pub(crate) version: Option<u64>,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) kind: String,
    pub(crate) tier: &'static str,
    pub(crate) tags: Vec<String>,
    pub(crate) expires_at: Option<String>,
}

/// One destination page and its versions, oldest first.
#[derive(Debug, Clone)]
pub(crate) struct PlannedPage {
    pub(crate) record: &'static str,
    pub(crate) destination: Destination,
    pub(crate) path: String,
    pub(crate) versions: Vec<PlannedVersion>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RunStatus {
    Planned,
    Imported,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VersionStatus {
    Planned,
    /// Written by this run.
    Written,
    /// Already on the server, written by an earlier run of this import.
    Present,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct VersionEntry {
    pub(crate) key: String,
    pub(crate) source_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<u64>,
    pub(crate) status: VersionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) page_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PageEntry {
    pub(crate) record: &'static str,
    pub(crate) workspace: String,
    pub(crate) project: String,
    pub(crate) path: String,
    pub(crate) versions: Vec<VersionEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct HookSessionEntry {
    pub(crate) source_session_id: String,
    pub(crate) stable_session_id: String,
    pub(crate) workspace: String,
    pub(crate) project: String,
    pub(crate) planned_events: usize,
    pub(crate) accepted_events: usize,
    pub(crate) status: RunStatus,
}

/// The durable record of one agentmemory import: what was planned, what was
/// left out and why, and the state of every page version.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Manifest {
    pub(crate) import_version: &'static str,
    pub(crate) source_path: String,
    pub(crate) source_sha256: String,
    pub(crate) status: RunStatus,
    /// Planned records per kind (`memories`, `memory_versions`, ...).
    pub(crate) planned: BTreeMap<String, usize>,
    /// Records left out, per reason. Nothing is dropped silently.
    pub(crate) dropped: BTreeMap<String, usize>,
    /// Source projects the mapping file does not cover, with record counts.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) unmapped_projects: BTreeMap<String, usize>,
    pub(crate) written_versions: usize,
    pub(crate) present_versions: usize,
    pub(crate) pages: Vec<PageEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) hook_sessions: Vec<HookSessionEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

impl Manifest {
    pub(crate) fn new(
        import_version: &'static str,
        source_path: String,
        source_sha256: String,
        pages: &[PlannedPage],
    ) -> Self {
        let mut planned = BTreeMap::new();
        for page in pages {
            *planned.entry(format!("{}_pages", page.record)).or_default() += 1;
            *planned
                .entry(format!("{}_versions", page.record))
                .or_default() += page.versions.len();
        }
        Self {
            import_version,
            source_path,
            source_sha256,
            status: RunStatus::Planned,
            planned,
            dropped: BTreeMap::new(),
            unmapped_projects: BTreeMap::new(),
            written_versions: 0,
            present_versions: 0,
            pages: pages
                .iter()
                .map(|page| PageEntry {
                    record: page.record,
                    workspace: page.destination.workspace.clone(),
                    project: page.destination.label().to_owned(),
                    path: page.path.clone(),
                    versions: page
                        .versions
                        .iter()
                        .map(|version| VersionEntry {
                            key: version.key.clone(),
                            source_id: version.source_id.clone(),
                            version: version.version,
                            status: VersionStatus::Planned,
                            page_id: None,
                            error: None,
                        })
                        .collect(),
                })
                .collect(),
            hook_sessions: Vec::new(),
            error: None,
        }
    }

    pub(crate) fn write(&self, path: &Path) -> Result<()> {
        crate::atomic_write(path, serde_json::to_string_pretty(self)?.as_bytes())
            .with_context(|| format!("write manifest {}", path.display()))
    }

    /// Record a failure and persist it; returns the error to propagate.
    pub(crate) fn fail(&mut self, path: &Path, error: anyhow::Error) -> anyhow::Error {
        self.status = RunStatus::Failed;
        self.error = Some(crate::mcp::preview(&format!("{error:#}")));
        if let Err(write_error) = self.write(path) {
            return error.context(format!("also failed to update the manifest: {write_error}"));
        }
        error
    }
}

/// Print the dry-run summary every agentmemory route shares.
pub(crate) fn print_plan(pages: &[PlannedPage], manifest: &Manifest, show_body: bool) {
    let versions: usize = pages.iter().map(|page| page.versions.len()).sum();
    println!(
        "dry-run: planned {} pages ({versions} page versions); no HTTP writes performed",
        pages.len()
    );
    for (key, count) in &manifest.planned {
        println!("  planned {key}: {count}");
    }
    for (key, count) in &manifest.dropped {
        println!("  dropped {key}: {count}");
    }
    for (project, count) in &manifest.unmapped_projects {
        println!("  UNMAPPED project {project:?}: {count} record(s); --apply refuses to run");
    }
    for page in pages {
        println!(
            "  {} -> {}/{}:{} ({} version{})",
            page.versions
                .last()
                .map_or("", |version| version.source_id.as_str()),
            page.destination.workspace,
            page.destination.label(),
            page.path,
            page.versions.len(),
            if page.versions.len() == 1 { "" } else { "s" }
        );
        if show_body {
            for version in &page.versions {
                println!("--- {} ---\n{}", version.key, version.body);
            }
        }
    }
}

/// Check the server and every destination before the first write.
pub(crate) async fn preflight(
    client: &ImportClient,
    destinations: &BTreeSet<Destination>,
    create_destination: bool,
) -> Result<()> {
    client.ensure_conditional_writes().await?;
    if create_destination {
        return Ok(());
    }
    for destination in destinations {
        if destination.project_id.is_some() {
            // The guarded batch resolver itself verifies that a stable id is
            // authorized and exists. `memory_recent` does not accept ids.
            continue;
        }
        if !client
            .mcp_scope_exists(&destination.workspace, &destination.project)
            .await
        {
            bail!(
                "destination {}/{} must already exist (or pass --create-destination)",
                destination.workspace,
                destination.label()
            );
        }
    }
    Ok(())
}

/// Write every planned page, versions oldest first, each conditional on the
/// version before it. Stops at the first failure; the manifest records every
/// version's state, and a rerun resumes from what the server holds.
pub(crate) async fn apply_pages_batched(
    client: &ImportClient,
    pages: &[PlannedPage],
    manifest: &mut Manifest,
    manifest_path: &Path,
) -> Result<()> {
    #[derive(Default)]
    struct PageState {
        index: usize,
        latest: Option<String>,
    }

    const MAX_BATCH_PAGES: usize = 100;
    const MAX_BATCH_BODY_BYTES: usize = 8 * 1024 * 1024;
    let mut states: Vec<PageState> = (0..pages.len()).map(|_| PageState::default()).collect();

    while let Some(first) = states
        .iter()
        .enumerate()
        .find(|(page_index, state)| state.index < pages[*page_index].versions.len())
        .map(|(index, _)| index)
    {
        let destination = &pages[first].destination;
        let mut pending = Vec::new();
        let mut body_bytes = 0usize;
        for (page_index, state) in states.iter().enumerate() {
            let page = &pages[page_index];
            if state.index >= page.versions.len() || page.destination != *destination {
                continue;
            }
            let next_bytes = page.versions[state.index].body.len();
            if !pending.is_empty()
                && (pending.len() == MAX_BATCH_PAGES
                    || body_bytes.saturating_add(next_bytes) > MAX_BATCH_BODY_BYTES)
            {
                break;
            }
            if next_bytes > MAX_BATCH_BODY_BYTES {
                let error = anyhow::anyhow!("{} exceeds the batch body limit", page.path);
                return Err(manifest.fail(manifest_path, error));
            }
            body_bytes += next_bytes;
            pending.push(page_index);
        }

        let writes: Vec<McpWrite<'_>> = pending
            .iter()
            .map(|&page_index| {
                let page = &pages[page_index];
                let version = &page.versions[states[page_index].index];
                McpWrite {
                    workspace: &page.destination.workspace,
                    project: &page.destination.project,
                    project_id: page.destination.project_id.as_deref(),
                    path: &page.path,
                    body: &version.body,
                    title: Some(&version.title),
                    kind: &version.kind,
                    tier: version.tier,
                    tags: &version.tags,
                    expires_at: version.expires_at.as_deref(),
                }
            })
            .collect();
        let batch: Vec<_> = writes
            .iter()
            .zip(pending.iter())
            .map(|(write, &page_index)| {
                let precondition = states[page_index]
                    .latest
                    .as_deref()
                    .map_or(Precondition::Absent, Precondition::Latest);
                (write, precondition)
            })
            .collect();

        match client.mcp_write_pages_batch(&batch).await {
            Ok(page_ids) => {
                for (&page_index, page_id) in pending.iter().zip(page_ids) {
                    let version_index = states[page_index].index;
                    let entry = &mut manifest.pages[page_index].versions[version_index];
                    entry.status = VersionStatus::Written;
                    entry.page_id = Some(page_id.clone());
                    manifest.written_versions += 1;
                    states[page_index].latest = Some(page_id);
                    states[page_index].index += 1;
                }
                manifest.write(manifest_path)?;
            }
            Err(error) if error.downcast_ref::<PreconditionFailed>().is_some() => {
                let failed = error.downcast_ref::<PreconditionFailed>().unwrap();
                let local_index = failed.index.unwrap_or(0);
                let Some(&page_index) = pending.get(local_index) else {
                    return Err(manifest.fail(
                        manifest_path,
                        anyhow::anyhow!("batch precondition failure named an invalid index"),
                    ));
                };
                let page = &pages[page_index];
                let current = match client
                    .mcp_read_page(
                        &page.destination.workspace,
                        &page.destination.project,
                        page.destination.project_id.as_deref(),
                        &page.path,
                    )
                    .await
                    .with_context(|| format!("reading {} after a refused batch", page.path))
                {
                    Ok(current) => current,
                    Err(error) => return Err(manifest.fail(manifest_path, error)),
                };
                if let Some(error) = current.unverifiable(&page.path) {
                    let entry = &mut manifest.pages[page_index].versions[states[page_index].index];
                    entry.status = VersionStatus::Failed;
                    entry.error = Some(crate::mcp::preview(&format!("{error:#}")));
                    return Err(manifest.fail(manifest_path, error));
                }
                let start = states[page_index].index;
                let Some(found) = page
                    .versions
                    .iter()
                    .rposition(|version| current.body == version.body)
                    .filter(|found| *found >= start)
                else {
                    let entry = &mut manifest.pages[page_index].versions[start];
                    entry.status = VersionStatus::Failed;
                    entry.error = Some("destination page was not written by this import".into());
                    let error = anyhow::anyhow!(
                        "{} already holds a page this import did not write; refusing to replace it",
                        page.path
                    );
                    return Err(manifest.fail(manifest_path, error));
                };
                for skipped in start..=found {
                    let entry = &mut manifest.pages[page_index].versions[skipped];
                    entry.status = VersionStatus::Present;
                    if skipped == found {
                        entry.page_id = current.page_id.clone();
                    }
                    manifest.present_versions += 1;
                }
                states[page_index].latest = current.page_id;
                states[page_index].index = found + 1;
                if states[page_index].latest.is_none()
                    && states[page_index].index < page.versions.len()
                {
                    let error = anyhow::anyhow!(
                        "{} has no version id; cannot continue its chain",
                        page.path
                    );
                    return Err(manifest.fail(manifest_path, error));
                }
                manifest.write(manifest_path)?;
            }
            Err(error) => {
                let page_index = pending[0];
                let entry = &mut manifest.pages[page_index].versions[states[page_index].index];
                entry.status = VersionStatus::Failed;
                entry.error = Some(crate::mcp::preview(&format!("{error:#}")));
                return Err(manifest.fail(manifest_path, error));
            }
        }
    }
    manifest.status = RunStatus::Imported;
    manifest.write(manifest_path)
}

/// Compatibility path for the established agentmemory routes. They retain
/// ai-memory 3.0 support; only claude-mem requires the newer batch tool.
pub(crate) async fn apply_pages(
    client: &ImportClient,
    pages: &[PlannedPage],
    manifest: &mut Manifest,
    manifest_path: &Path,
) -> Result<()> {
    for (page_index, page) in pages.iter().enumerate() {
        if let Err(error) = apply_page(client, page, page_index, manifest, manifest_path).await {
            return Err(manifest.fail(manifest_path, error));
        }
    }
    manifest.status = RunStatus::Imported;
    manifest.write(manifest_path)
}

async fn apply_page(
    client: &ImportClient,
    page: &PlannedPage,
    page_index: usize,
    manifest: &mut Manifest,
    manifest_path: &Path,
) -> Result<()> {
    let mut latest: Option<String> = None;
    let mut index = 0;
    while index < page.versions.len() {
        let version = &page.versions[index];
        let write = McpWrite {
            workspace: &page.destination.workspace,
            project: &page.destination.project,
            project_id: page.destination.project_id.as_deref(),
            path: &page.path,
            body: &version.body,
            title: Some(&version.title),
            kind: &version.kind,
            tier: version.tier,
            tags: &version.tags,
            expires_at: version.expires_at.as_deref(),
        };
        let precondition = latest
            .as_deref()
            .map_or(Precondition::Absent, Precondition::Latest);
        match client.mcp_write_page(&write, precondition).await {
            Ok(page_id) => {
                let entry = &mut manifest.pages[page_index].versions[index];
                entry.status = VersionStatus::Written;
                entry.page_id = Some(page_id.clone());
                manifest.written_versions += 1;
                manifest.write(manifest_path)?;
                latest = Some(page_id);
                index += 1;
            }
            Err(error) if error.downcast_ref::<PreconditionFailed>().is_some() => {
                let current = client
                    .mcp_read_page(
                        &page.destination.workspace,
                        &page.destination.project,
                        page.destination.project_id.as_deref(),
                        &page.path,
                    )
                    .await
                    .with_context(|| format!("reading {} after a refused write", page.path))?;
                if let Some(error) = current.unverifiable(&page.path) {
                    let entry = &mut manifest.pages[page_index].versions[index];
                    entry.status = VersionStatus::Failed;
                    entry.error = Some(crate::mcp::preview(&format!("{error:#}")));
                    return Err(error);
                }
                let Some(found) = page
                    .versions
                    .iter()
                    .rposition(|version| current.body == version.body)
                    .filter(|found| *found >= index)
                else {
                    let entry = &mut manifest.pages[page_index].versions[index];
                    entry.status = VersionStatus::Failed;
                    entry.error = Some("destination page was not written by this import".into());
                    bail!(
                        "{} already holds a page this import did not write; refusing to replace it",
                        page.path
                    );
                };
                for skipped in index..=found {
                    let entry = &mut manifest.pages[page_index].versions[skipped];
                    entry.status = VersionStatus::Present;
                    if skipped == found {
                        entry.page_id = current.page_id.clone();
                    }
                    manifest.present_versions += 1;
                }
                manifest.write(manifest_path)?;
                latest = current.page_id;
                index = found + 1;
                if latest.is_none() && index < page.versions.len() {
                    bail!("{} has no version id; cannot continue its chain", page.path);
                }
            }
            Err(error) => {
                let entry = &mut manifest.pages[page_index].versions[index];
                entry.status = VersionStatus::Failed;
                entry.error = Some(crate::mcp::preview(&format!("{error:#}")));
                return Err(error.context(format!("writing {}", page.path)));
            }
        }
    }
    Ok(())
}

/// Print the closing line of a live run.
pub(crate) fn print_done(manifest: &Manifest) {
    println!(
        "import complete: wrote {} page version(s); {} were already present",
        manifest.written_versions, manifest.present_versions
    );
}

#[cfg(test)]
pub(crate) mod e2e;
#[cfg(test)]
pub(crate) mod mock;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dest_prefix_refuses_traversal_absolute_and_reserved_families() {
        assert_eq!(
            validate_dest_prefix("imported/agentmemory/").unwrap(),
            "imported/agentmemory"
        );
        assert_eq!(
            validate_dest_prefix("archive/am-2026").unwrap(),
            "archive/am-2026"
        );
        for bad in [
            "",
            "/abs/agentmemory",
            "../escape",
            "imported/../sessions",
            "imported/./x",
            "imported//x",
            "C:\\vault",
            "c:/vault",
            "sessions",
            "sessions/agentmemory",
            "_rules",
            "_prompts/x",
            "_global",
            ".git",
            "decisions/am",
            "gotchas",
            "procedures",
            "profile",
            "Imported/Upper",
            "imported/space here",
            "imported/.hidden",
        ] {
            assert!(
                validate_dest_prefix(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn planned_paths_must_stay_under_the_prefix() {
        let prefix = "imported/agentmemory";
        validate_planned_path("imported/agentmemory/memories/a.md", prefix).unwrap();
        for bad in [
            "imported/agentmemory.md",
            "imported/agentmemory/../x.md",
            "imported/agentmemoryx/a.md",
            "sessions/a.md",
            "/imported/agentmemory/a.md",
            "imported/agentmemory/a",
        ] {
            assert!(
                validate_planned_path(bad, prefix).is_err(),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn ingest_keys_are_stable_uuid_v5_and_distinct_per_record() {
        let key = ingest_key("memory", "mem_1");
        assert_eq!(key, ingest_key("memory", "mem_1"));
        let parsed = Uuid::parse_str(&key).unwrap();
        assert_eq!(parsed.get_version_num(), 5);
        assert_ne!(key, ingest_key("memory", "mem_2"));
        assert_ne!(key, ingest_key("lesson", "mem_1"));
    }

    #[test]
    fn ids_that_look_like_credentials_or_carry_controls_are_refused() {
        validate_id("mem_01HX", "memory").unwrap();
        assert!(validate_id("", "memory").is_err());
        assert!(validate_id("a\nb", "memory").is_err());
        assert!(validate_id(&"x".repeat(MAX_ID_BYTES + 1), "memory").is_err());
        assert!(validate_id("sk-1234567890abcdefghijklmnop", "memory").is_err());
    }

    #[test]
    fn session_ids_display_whole_up_to_the_bound_and_truncated_past_it() {
        assert_eq!(display_session_id("ses_1"), "ses_1");
        let exact = "s".repeat(64);
        assert_eq!(display_session_id(&exact), exact);
        let long = format!("{exact}tail");
        assert_eq!(display_session_id(&long), format!("{exact}..."));
    }

    #[test]
    fn wikilinks_resolve_or_fall_back_to_plain_text() {
        let resolve = |target: &str| {
            (target == "mem_a" || target == "memories/mem_a")
                .then(|| "imported/agentmemory/memories/mem-a".to_owned())
        };
        assert_eq!(
            rewrite_wikilinks("see [[mem_a]] and [[memories/mem_a|A title]]", resolve),
            "see [[imported/agentmemory/memories/mem-a]] and [[imported/agentmemory/memories/mem-a|A title]]"
        );
        assert_eq!(
            rewrite_wikilinks(
                "gone: [[mem_zz]], alias [[x|Shown]], evil [[../../_rules/x]]",
                resolve
            ),
            "gone: mem_zz, alias Shown, evil ../../_rules/x"
        );
        assert_eq!(
            rewrite_wikilinks("open [[ never closed", resolve),
            "open [[ never closed"
        );
    }

    #[test]
    fn tags_are_bounded_deduplicated_and_led_by_the_import_tag() {
        let raw: Vec<String> = (0..40).map(|i| format!("concept {}", i % 20)).collect();
        let tags = tags_from(raw.iter().map(String::as_str));
        assert_eq!(tags[0], IMPORT_TAG);
        assert_eq!(tags.len(), MAX_TAGS);
        let unique: BTreeSet<_> = tags.iter().collect();
        assert_eq!(unique.len(), tags.len());
    }

    #[test]
    fn memory_types_map_to_kind_and_tier() {
        assert_eq!(memory_kind_tier("architecture"), ("decision", "semantic"));
        assert_eq!(memory_kind_tier("workflow"), ("workflow", "procedural"));
        assert_eq!(memory_kind_tier("pattern"), ("pattern", "procedural"));
        assert_eq!(memory_kind_tier("something-new"), ("memory", "semantic"));
    }
}
