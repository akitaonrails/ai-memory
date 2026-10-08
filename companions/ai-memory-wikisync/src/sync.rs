//! The plan/export engine: three-way classification and batched writes.
//!
//! For every allowlisted page the tool compares three versions — the
//! destination file (mine), the state entry (base: what this tool last
//! wrote), and the server body (theirs, revalidated through the ETag) — and
//! refuses to choose a winner when the destination has diverged from both.
//! Writes happen only in one batch per run, and the state file is replaced
//! atomically after the whole batch succeeds, so a crash at any point leaves
//! a consistent (old) state that the next run re-classifies safely.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::client::{ApiClient, ApiPage, PageSummary};
use crate::page_file::{self, PageMeta};
use crate::paths;
use crate::state::{self, PageState, SyncState};
use crate::{MAX_BODY_BYTES, MAX_PAGES};

/// Why a planned write touches the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    /// No destination file: the server body creates one.
    Create,
    /// Destination still equals the base state: fast-forward to the server.
    Update,
    /// Destination diverged and `--force` chose the server version.
    Forced,
}

impl WriteKind {
    fn label(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Forced => "forced",
        }
    }
}

/// Why a destination file refuses to be overwritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DivergentReason {
    /// Edited locally since the last export.
    LocalEdit,
    /// Present on disk but never written by this tool.
    UnknownFile,
}

impl DivergentReason {
    fn describe(self) -> &'static str {
        match self {
            Self::LocalEdit => "edited locally since the last export",
            Self::UnknownFile => "exists on disk but was never written by wikisync",
        }
    }
}

/// What a page's classification means for the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Destination matches the server byte-for-byte: nothing to do.
    Unchanged,
    /// Safe to write the server body (see [`WriteKind`]).
    Write(WriteKind),
    /// Destination differs from both base and server: a local edit this
    /// tool refuses to overwrite without `--force`.
    Divergent(DivergentReason),
}

/// One planned or performed write.
#[derive(Debug, Clone)]
pub struct PlannedWrite {
    pub kind: WriteKind,
    pub path: String,
    pub title: String,
    pub body: String,
    pub etag: Option<String>,
}

/// A destination file that diverged, with a bounded diff summary.
#[derive(Debug, Clone)]
pub struct Divergence {
    pub path: String,
    pub reason: DivergentReason,
    pub summary: String,
}

/// Everything one run decided, for printing (and for tests).
#[derive(Debug, Default)]
pub struct Report {
    pub writes: Vec<PlannedWrite>,
    pub unchanged: Vec<String>,
    pub divergent: Vec<Divergence>,
    pub listed: usize,
    pub matched: usize,
    /// Total bytes the batch would write.
    pub bytes_to_write: u64,
}

impl Report {
    fn count(&self, kind: WriteKind) -> usize {
        self.writes
            .iter()
            .filter(|write| write.kind == kind)
            .count()
    }
}

/// How the run treats the filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `plan`: never writes anything, not even the state file.
    Plan,
    /// `export` without `--apply`: still a dry-run.
    DryRun,
    /// `export --apply`.
    Apply,
}

/// Arguments shared by `plan` and `export`.
#[derive(Debug, Clone)]
pub struct RunArgs {
    pub server: String,
    pub token: Option<String>,
    pub workspace: String,
    pub project: String,
    pub dest: PathBuf,
    /// Explicit family allowlist. Must be non-empty; `*` is refused.
    pub include: Vec<String>,
    /// Overwrite files that diverged locally since the last export.
    pub force: bool,
}

/// Classify one page from its three versions. Pure: all IO happens around
/// it, which is what makes the three-way rules testable.
pub fn classify(
    disk: Option<&[u8]>,
    base: Option<&PageState>,
    server: Option<&[u8]>,
) -> Classification {
    let disk_hash = disk.map(state::sha256_hex);
    let server_hash = server.map(state::sha256_hex);
    // Order matters: a byte-identical server match is Unchanged even with
    // no base entry (the file may be left over from an interrupted run
    // after the write but before the state save).
    let disk_equals_server = disk_hash.is_some() && disk_hash == server_hash;
    let disk_equals_base = match (&disk_hash, base) {
        (Some(disk), Some(base)) => disk == &base.hash,
        _ => false,
    };
    match (disk, base) {
        (None, _) => Classification::Write(WriteKind::Create),
        (Some(_), _) if disk_equals_server => Classification::Unchanged,
        (Some(_), Some(_)) if disk_equals_base => match server {
            // Server unchanged since the base (304): the file is current.
            None => Classification::Unchanged,
            Some(_) => Classification::Write(WriteKind::Update),
        },
        (Some(_), None) => Classification::Divergent(DivergentReason::UnknownFile),
        (Some(_), Some(_)) => Classification::Divergent(DivergentReason::LocalEdit),
    }
}

/// The file bytes for a server page: its round-trippable metadata as
/// frontmatter, then the body verbatim. Export and sync write the same
/// bytes, so the two never disagree about whether a file is current.
pub fn render_page(page: &ApiPage) -> Result<String> {
    if page.body_markdown.len() > MAX_BODY_BYTES {
        bail!(
            "page {} is {} bytes; refusing bodies over {MAX_BODY_BYTES} bytes",
            page.path,
            page.body_markdown.len()
        );
    }
    let meta = PageMeta::from_server(&page.title, &page.tier, page.pinned, &page.frontmatter);
    Ok(page_file::render(&meta, &page.body_markdown))
}

/// Bounded, printable diff summary for a divergent file: sizes, line counts
/// and the first differing line, with control characters neutralised so a
/// page body cannot smuggle terminal escapes into the output.
pub fn diff_summary(disk: Option<&[u8]>, server: Option<&[u8]>) -> String {
    let Some(disk) = disk else {
        return "no local file".to_string();
    };
    let Some(server) = server else {
        return "server content unchanged since the last export (revalidated by ETag)".to_string();
    };
    let local_lines = disk.iter().filter(|byte| **byte == b'\n').count();
    let server_lines = server.iter().filter(|byte| **byte == b'\n').count();
    let first_difference = disk
        .split(|byte| *byte == b'\n')
        .zip(server.split(|byte| *byte == b'\n'))
        .position(|(local, remote)| local != remote)
        .map_or(0, |index| index + 1);
    let local_line = disk
        .split(|byte| *byte == b'\n')
        .nth(first_difference.saturating_sub(1))
        .unwrap_or_default();
    let server_line = server
        .split(|byte| *byte == b'\n')
        .nth(first_difference.saturating_sub(1))
        .unwrap_or_default();
    format!(
        "local {} bytes/{} lines vs server {} bytes/{} lines; first difference at line \
         {}: local {:?} vs server {:?}",
        disk.len(),
        local_lines,
        server.len(),
        server_lines,
        first_difference,
        preview(local_line),
        preview(server_line),
    )
}

fn preview(line: &[u8]) -> String {
    line.iter()
        .take(60)
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                '?'
            }
        })
        .collect()
}

/// Atomic file replacement inside the destination: write a hidden tmp file
/// next to the target, fsync, rename. The tmp name is namespaced by this
/// tool so a crash never leaves an ambiguous artifact next to page files.
pub fn atomic_write(dest_root: &Path, rel: &str, bytes: &[u8]) -> Result<()> {
    let target = paths::secure_join(dest_root, rel)?;
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create directory {}", parent.display()))?;
    }
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "page.md".to_string());
    let tmp = target
        .parent()
        .unwrap_or(dest_root)
        .join(format!(".wikisync-tmp-{file_name}"));
    {
        let mut file =
            fs::File::create(&tmp).with_context(|| format!("cannot create {}", tmp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("cannot sync {}", tmp.display()))?;
    }
    fs::rename(&tmp, &target)
        .with_context(|| format!("cannot move {} into place", tmp.display()))?;
    Ok(())
}

/// Validate the allowlist: explicit, concrete families only.
pub fn validate_allowlist(include: &[String]) -> Result<Vec<String>> {
    if include.is_empty() {
        bail!(
            "at least one --include FAMILY is required; the family allowlist must be \
             explicit (for example --include _rules --include decisions)"
        );
    }
    let mut families = Vec::new();
    for family in include {
        paths::validate_family(family)?;
        if !families.contains(family) {
            families.push(family.clone());
        }
    }
    Ok(families)
}

/// Select, validate and bound the server listing down to the allowlisted
/// families. Deduplicates by path: incremental paging can legitimately
/// re-deliver a page that was updated mid-iteration.
pub fn select_pages(summaries: Vec<PageSummary>, families: &[String]) -> Result<Vec<PageSummary>> {
    let mut by_path: std::collections::BTreeMap<String, PageSummary> =
        std::collections::BTreeMap::new();
    for summary in summaries {
        if !families
            .iter()
            .any(|family| paths::family_of(&summary.path) == family.as_str())
        {
            continue;
        }
        paths::validate_page_path(&summary.path)?;
        by_path.insert(summary.path.clone(), summary);
    }
    let selected: Vec<PageSummary> = by_path.into_values().collect();
    if selected.len() > MAX_PAGES {
        bail!(
            "{} allowlisted pages exceed the per-run ceiling of {MAX_PAGES}; narrow --include",
            selected.len()
        );
    }
    let paths: Vec<String> = selected.iter().map(|page| page.path.clone()).collect();
    paths::ensure_no_case_fold_collisions(&paths)?;
    Ok(selected)
}

pub(crate) fn read_disk(dest_root: &Path, path: &str) -> Result<Option<Vec<u8>>> {
    let target = paths::secure_join(dest_root, path)?;
    match fs::read(&target) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e)).context(format!("cannot read {}", target.display())),
    }
}

/// Fetch and classify every allowlisted page. In [`Mode::Apply`] (and only
/// when allowed past the divergence guard) also performs the write batch
/// and persists the new state atomically.
async fn run_batch(
    client: &ApiClient,
    args: &RunArgs,
    dest_root: &Path,
    old_state: &SyncState,
    mode: Mode,
) -> Result<(Report, SyncState, bool)> {
    let summaries = client.list_pages(&args.workspace, &args.project).await?;
    let listed = summaries.len();
    let selected = select_pages(summaries, &args.include)?;

    let mut report = Report {
        listed,
        matched: selected.len(),
        ..Report::default()
    };
    let mut new_state = old_state.clone();

    for page in selected {
        let disk = read_disk(dest_root, &page.path)?;
        let base = old_state.pages.get(&page.path);
        // A 304 can only prove something when there is a file to compare
        // against; a missing file always needs the full body.
        let etag_hint = if disk.is_some() {
            base.and_then(|entry| entry.etag.as_deref())
        } else {
            None
        };
        let read = client
            .read_page(&args.workspace, &args.project, &page.path, etag_hint)
            .await?;
        let mut body = read.page.as_ref().map(render_page).transpose()?;
        let mut decision = classify(
            disk.as_deref(),
            base,
            body.as_deref().map(|b: &str| b.as_bytes()),
        );
        if matches!(decision, Classification::Divergent(_)) && args.force && body.is_none() {
            // A 304 proves the server is unchanged, but --force still needs
            // the bytes to overwrite the divergent file.
            let refetch = client
                .read_page(&args.workspace, &args.project, &page.path, None)
                .await?;
            body = refetch.page.as_ref().map(render_page).transpose()?;
            decision = classify(
                disk.as_deref(),
                base,
                body.as_deref().map(|b: &str| b.as_bytes()),
            );
        }
        record_decision(
            &mut report,
            &mut new_state,
            page,
            disk.as_deref(),
            body,
            read.etag,
            decision,
            args.force,
        )?;
    }

    report.bytes_to_write = report
        .writes
        .iter()
        .map(|write| write.body.len() as u64)
        .sum();

    let applied = mode == Mode::Apply && (report.divergent.is_empty() || args.force);
    if applied {
        for write in &report.writes {
            atomic_write(dest_root, &write.path, write.body.as_bytes())?;
        }
        // Only after the whole batch is on disk does the state move: a
        // crash mid-batch leaves the old state, and the next run's
        // classification adopts any file that already matches the server.
        state::save(dest_root, &new_state)?;
    }
    Ok((report, new_state, applied))
}

#[allow(clippy::too_many_arguments)]
fn record_decision(
    report: &mut Report,
    new_state: &mut SyncState,
    page: PageSummary,
    disk: Option<&[u8]>,
    body: Option<String>,
    etag: Option<String>,
    decision: Classification,
    force: bool,
) -> Result<()> {
    match decision {
        Classification::Unchanged => {
            if let Some(body) = &body {
                // The file already equals the server (for example after a
                // crash between write and state save): adopt it.
                new_state.pages.insert(
                    page.path.clone(),
                    PageState {
                        hash: state::sha256_hex(body.as_bytes()),
                        etag,
                    },
                );
            }
            report.unchanged.push(page.path.clone());
        }
        Classification::Write(kind) => {
            let Some(body) = body else {
                bail!(
                    "server returned 304 Not Modified for {} although no destination \
                     file exists; cannot classify the page",
                    page.path
                );
            };
            new_state.pages.insert(
                page.path.clone(),
                PageState {
                    hash: state::sha256_hex(body.as_bytes()),
                    etag: etag.clone(),
                },
            );
            report.writes.push(PlannedWrite {
                kind,
                path: page.path.clone(),
                title: page.title,
                body,
                etag,
            });
        }
        Classification::Divergent(reason) => {
            let summary = diff_summary(disk, body.as_deref().map(|b: &str| b.as_bytes()));
            report.divergent.push(Divergence {
                path: page.path.clone(),
                reason,
                summary,
            });
            if force {
                // --force only ever overwrites with server content; the
                // tool still never deletes anything.
                if let Some(body) = &body {
                    new_state.pages.insert(
                        page.path.clone(),
                        PageState {
                            hash: state::sha256_hex(body.as_bytes()),
                            etag: etag.clone(),
                        },
                    );
                    report.writes.push(PlannedWrite {
                        kind: WriteKind::Forced,
                        path: page.path.clone(),
                        title: page.title.clone(),
                        body: body.clone(),
                        etag,
                    });
                }
            }
        }
    }
    Ok(())
}

fn print_report(args: &RunArgs, report: &Report, mode: Mode, families: &[String], applied: bool) {
    println!(
        "ai-memory-wikisync: {}/{} -> {}",
        args.workspace,
        args.project,
        args.dest.display()
    );
    println!(
        "  allowlist: {}; server listed {} page(s), {} in allowlisted families",
        families.join(", "),
        report.listed,
        report.matched
    );
    for write in &report.writes {
        println!(
            "  {:<9} {} ({} bytes) {:?}",
            write.kind.label(),
            write.path,
            write.body.len(),
            write.title
        );
    }
    for path in &report.unchanged {
        println!("  {:<9} {path}", "unchanged");
    }
    for divergence in &report.divergent {
        println!(
            "  {:<9} {} — {} ({})",
            "REFUSED",
            divergence.path,
            divergence.reason.describe(),
            divergence.summary
        );
    }
    println!(
        "  plan: {} create, {} update, {} unchanged, {} refused; {} bytes to write",
        report.count(WriteKind::Create),
        report.count(WriteKind::Update),
        report.unchanged.len(),
        report.divergent.len(),
        report.bytes_to_write
    );
    match mode {
        Mode::Plan => println!("  dry run: `plan` never writes files or state"),
        Mode::DryRun => println!("  dry run: no files were written (pass --apply to export)"),
        Mode::Apply if applied => {}
        Mode::Apply => println!("  nothing was written"),
    }
}

pub(crate) fn print_git_hints(args: &RunArgs) {
    println!(
        "this tool never runs git; to commit the export you may run:\
         \n  git add -- {}\
         \n  git commit -m \"chore(wikisync): export team wiki from {}/{}\"\
         \n  git push",
        args.dest.display(),
        args.workspace,
        args.project
    );
}

/// Validate the destination root. `Apply` creates a missing directory;
/// dry-run modes never touch the filesystem, so for them a missing
/// destination is simply an empty destination (validated only lexically).
pub(crate) fn resolve_dest(dest: &Path, mode: Mode) -> Result<PathBuf> {
    match mode {
        Mode::Apply => paths::prepare_dest(dest),
        Mode::Plan | Mode::DryRun => {
            if dest.exists() {
                paths::prepare_dest(dest)
            } else {
                Ok(paths::lexical_absolute(dest))
            }
        }
    }
}

/// Entry point behind both CLI subcommands.
pub async fn run(args: &RunArgs, mode: Mode) -> Result<()> {
    let families = validate_allowlist(&args.include)?;
    let dest_root = resolve_dest(&args.dest, mode)?;
    let old_state = state::load(&dest_root)?;
    let client = ApiClient::new(&args.server, args.token.clone())?;

    let (report, _new_state, applied) =
        run_batch(&client, args, &dest_root, &old_state, mode).await?;

    print_report(args, &report, mode, &families, applied);

    if mode == Mode::Apply && !report.divergent.is_empty() && !args.force {
        bail!(
            "{} file(s) diverged locally since the last export; nothing was written. \
             Inspect the diff summaries above, restore the files, or re-run with --force",
            report.divergent.len()
        );
    }
    if mode == Mode::Apply && applied {
        print_git_hints(args);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(hash: &str, etag: Option<&str>) -> PageState {
        PageState {
            hash: hash.to_string(),
            etag: etag.map(str::to_owned),
        }
    }

    #[test]
    fn classify_three_way_matrix() {
        let disk = b"# page\nbody\n";
        let disk_hash = state::sha256_hex(disk);
        let server = b"# page\nbody\n";
        let edited = b"# page\nedited\n";

        // No file: always a create, even when the server is unchanged.
        assert_eq!(
            classify(None, None, Some(server)),
            Classification::Write(WriteKind::Create)
        );
        assert_eq!(
            classify(None, Some(&base(&disk_hash, None)), Some(server)),
            Classification::Write(WriteKind::Create)
        );

        // File equals server: unchanged, with or without a base entry.
        assert_eq!(
            classify(Some(disk), None, Some(server)),
            Classification::Unchanged
        );
        assert_eq!(
            classify(Some(disk), Some(&base("other", None)), Some(server)),
            Classification::Unchanged
        );

        // File equals base: 304 keeps it current, a new body fast-forwards.
        assert_eq!(
            classify(Some(disk), Some(&base(&disk_hash, None)), None),
            Classification::Unchanged
        );
        assert_eq!(
            classify(Some(disk), Some(&base(&disk_hash, None)), Some(edited)),
            Classification::Write(WriteKind::Update)
        );

        // File differs from base and server: divergent, never a winner.
        assert_eq!(
            classify(Some(edited), Some(&base(&disk_hash, None)), Some(server)),
            Classification::Divergent(DivergentReason::LocalEdit)
        );
        assert_eq!(
            classify(Some(edited), Some(&base(&disk_hash, None)), None),
            Classification::Divergent(DivergentReason::LocalEdit)
        );
        assert_eq!(
            classify(Some(edited), None, Some(server)),
            Classification::Divergent(DivergentReason::UnknownFile)
        );
    }

    #[test]
    fn diff_summary_points_at_first_difference() {
        let disk = b"one\ntwo local\nthree\n";
        let server = b"one\ntwo server\nthree\n";
        let summary = diff_summary(Some(disk), Some(server));
        assert!(summary.contains("line 2"), "{summary}");
        assert!(summary.contains("local"), "{summary}");
        let revalidated = diff_summary(Some(disk), None);
        assert!(revalidated.contains("revalidated by ETag"), "{revalidated}");
    }

    #[test]
    fn atomic_write_replaces_and_never_deletes_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        atomic_write(&root, "notes/a.md", b"v1").unwrap();
        assert_eq!(fs::read(root.join("notes/a.md")).unwrap(), b"v1");
        atomic_write(&root, "notes/a.md", b"v2").unwrap();
        assert_eq!(fs::read(root.join("notes/a.md")).unwrap(), b"v2");
        // No tmp artifacts left next to the page.
        let names: Vec<String> = fs::read_dir(root.join("notes"))
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.md".to_string()]);
    }

    #[test]
    fn allowlist_must_be_explicit() {
        assert!(validate_allowlist(&[]).is_err());
        let err = validate_allowlist(&["*".to_string()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("must be explicit"), "{err}");
        let both = validate_allowlist(&[
            "notes".to_string(),
            "notes".to_string(),
            "decisions".to_string(),
        ])
        .unwrap();
        assert_eq!(both, vec!["notes".to_string(), "decisions".to_string()]);
    }

    fn summary(path: &str) -> PageSummary {
        PageSummary {
            path: path.to_string(),
            title: format!("title {path}"),
            kind: "note".to_string(),
            tier: "semantic".to_string(),
            updated_at: "2026-10-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn select_pages_filters_families_and_dedupes() {
        let families = vec!["notes".to_string()];
        let selected = select_pages(
            vec![
                summary("notes/a.md"),
                summary("notes/a.md"),
                summary("decisions/skip.md"),
                summary("notes/b.md"),
            ],
            &families,
        )
        .unwrap();
        let paths: Vec<&str> = selected.iter().map(|p| p.path.as_str()).collect();
        assert_eq!(paths, vec!["notes/a.md", "notes/b.md"]);

        // A hostile server cannot smuggle traversal through the listing.
        let err = select_pages(vec![summary("notes/../escape.md")], &families)
            .unwrap_err()
            .to_string();
        assert!(err.contains("refused"), "{err}");
    }
}
