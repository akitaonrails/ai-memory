//! Two-way sync (#986, slice 3): repository edits flow back to the server
//! through the public `memory_write_page` tool, server edits flow out as in
//! `export`.
//!
//! Every page in an allowlisted family is classified from three versions:
//! the repository file, the state entry (what the last sync wrote), and the
//! server page rendered into file bytes. A change on one side is applied to
//! the other; a change on both sides is a conflict that only `--prefer`
//! resolves. Deletes on either side are reported and left alone (slice 4).
//!
//! Nothing is written unless `--apply` is passed and nothing in the plan is
//! refused. Before each import the server page is read again and must still
//! render to the bytes it was classified against: the MCP write has no
//! compare-and-write, so that re-read narrows the race to the gap between
//! one read and one write (the case for slice 2).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::client::{ApiClient, ApiPage};
use crate::mcp::McpClient;
use crate::page_file::{self, PageMeta};
use crate::state::{self, PageState, SyncState};
use crate::sync::{self, Mode, RunArgs};
use crate::{MAX_BODY_BYTES, MAX_PAGES, paths};

/// Server frontmatter keys a `memory_write_page` import writes back, or
/// the server regenerates on every write. A page carrying any other key
/// (a consolidated page's `summary`, `sources`, `kind`, …) would lose it,
/// because the tool clears what the call omits.
const RECREATED_KEYS: &[&str] = &[
    "title",
    "tier",
    "pinned",
    "tags",
    "type",
    "generated",
    "last_modified_by",
];

/// Which side wins a page changed on both sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefer {
    Repo,
    Server,
}

/// What the server holds for one path.
#[derive(Debug, Clone, Copy)]
pub enum ServerSide<'a> {
    /// Not in the listing: never created, or deleted.
    Absent,
    /// `304`: unchanged since the ETag recorded with the base.
    NotModified,
    /// The page rendered into file bytes.
    Rendered(&'a [u8]),
}

/// The decision for one page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Unchanged,
    /// Server to repository.
    Export {
        create: bool,
    },
    /// Repository to server.
    Import {
        create: bool,
    },
    /// Changed on both sides, and no `--prefer`.
    Conflict,
    /// The server no longer has a page the last sync saw (slice 4).
    DeletedOnServer,
    /// The repository no longer has a file the last sync wrote (slice 4).
    DeletedInRepo,
    /// Gone from both sides: only the state entry remains, and it is dropped.
    GoneOnBothSides,
}

/// Decide one page from its three versions. Pure, so the matrix is testable
/// without a server.
pub fn decide(
    disk: Option<&[u8]>,
    base: Option<&PageState>,
    server: ServerSide<'_>,
    prefer: Option<Prefer>,
) -> Action {
    let disk_hash = disk.map(state::sha256_hex);
    let disk_is_base = matches!((&disk_hash, base), (Some(disk), Some(base)) if *disk == base.hash);
    match server {
        ServerSide::Absent => match (disk.is_some(), base.is_some()) {
            (true, false) => Action::Import { create: true },
            (true, true) => Action::DeletedOnServer,
            (false, true) => Action::GoneOnBothSides,
            (false, false) => Action::Unchanged,
        },
        ServerSide::NotModified => match (disk.is_some(), base.is_some()) {
            (false, true) => Action::DeletedInRepo,
            (false, false) => Action::Unchanged,
            (true, _) if disk_is_base => Action::Unchanged,
            (true, _) => Action::Import { create: false },
        },
        ServerSide::Rendered(server) => {
            let server_hash = state::sha256_hex(server);
            match disk_hash {
                None if base.is_some() => Action::DeletedInRepo,
                None => Action::Export { create: true },
                Some(disk) if disk == server_hash => Action::Unchanged,
                Some(_) if disk_is_base => Action::Export { create: false },
                Some(_) if base.is_some_and(|base| base.hash == server_hash) => {
                    Action::Import { create: false }
                }
                Some(_) => match prefer {
                    Some(Prefer::Repo) => Action::Import { create: false },
                    Some(Prefer::Server) => Action::Export { create: false },
                    None => Action::Conflict,
                },
            }
        }
    }
}

/// Server frontmatter keys an import would clear, sorted.
pub fn keys_an_import_would_clear(frontmatter: &Value) -> Vec<String> {
    let Some(map) = frontmatter.as_object() else {
        return Vec::new();
    };
    let mut keys: Vec<String> = map
        .keys()
        .filter(|key| !RECREATED_KEYS.contains(&key.as_str()))
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// Arguments of `sync`.
#[derive(Debug, Clone)]
pub struct SyncArgs {
    pub run: RunArgs,
    pub prefer: Option<Prefer>,
}

struct Import {
    path: String,
    create: bool,
    meta: PageMeta,
    body: String,
    disk: Vec<u8>,
    /// Hash of the server page's rendering at classification, which must
    /// still hold right before the write; `None` for a create.
    expected_server: Option<String>,
}

struct Export {
    path: String,
    create: bool,
    bytes: String,
    etag: Option<String>,
}

/// A page the plan refuses to touch; any of these blocks `--apply`.
struct Refusal {
    path: String,
    reason: String,
}

#[derive(Default)]
struct Plan {
    imports: Vec<Import>,
    exports: Vec<Export>,
    unchanged: Vec<String>,
    refused: Vec<Refusal>,
    notices: Vec<(String, &'static str)>,
    /// State to keep for pages this run leaves alone or adopts.
    adopted: BTreeMap<String, PageState>,
    forget: Vec<String>,
}

/// Entry point behind `sync`.
pub async fn run(args: &SyncArgs, apply: bool) -> Result<()> {
    let run_args = &args.run;
    let families = sync::validate_allowlist(&run_args.include)?;
    let mode = if apply { Mode::Apply } else { Mode::DryRun };
    let dest_root = sync::resolve_dest(&run_args.dest, mode)?;
    let old_state = state::load(&dest_root)?;
    let api = ApiClient::new(&run_args.server, run_args.token.clone())?;

    let plan = build_plan(&api, args, &families, &dest_root, &old_state).await?;
    print_plan(run_args, &families, &plan, apply);

    if !apply {
        return Ok(());
    }
    if !plan.refused.is_empty() {
        bail!(
            "{} page(s) refused; nothing was written. Resolve them (or pass --prefer for \
             conflicts) and run again",
            plan.refused.len()
        );
    }
    let mcp = McpClient::new(&run_args.server, run_args.token.clone())?;
    let wrote_files = apply_plan(&api, &mcp, run_args, &dest_root, &old_state, plan).await?;
    if wrote_files {
        sync::print_git_hints(run_args);
    }
    Ok(())
}

async fn build_plan(
    api: &ApiClient,
    args: &SyncArgs,
    families: &[String],
    dest_root: &Path,
    old_state: &SyncState,
) -> Result<Plan> {
    let run_args = &args.run;
    let mut plan = Plan::default();
    let listed = api
        .list_pages(&run_args.workspace, &run_args.project)
        .await?;
    let on_server: BTreeSet<String> = sync::select_pages(listed, families)?
        .into_iter()
        .map(|page| page.path)
        .collect();
    let in_repo = local_pages(dest_root, families, &mut plan.refused)?;
    let in_state = old_state
        .pages
        .keys()
        .filter(|path| {
            families
                .iter()
                .any(|family| paths::family_of(path) == family)
        })
        .cloned();
    let all: BTreeSet<String> = on_server
        .iter()
        .cloned()
        .chain(in_repo)
        .chain(in_state)
        .collect();
    if all.len() > MAX_PAGES {
        bail!(
            "{} pages exceed the per-run ceiling of {MAX_PAGES}; narrow --include",
            all.len()
        );
    }
    paths::ensure_no_case_fold_collisions(&all.iter().cloned().collect::<Vec<_>>())?;

    for path in all {
        let disk = sync::read_disk(dest_root, &path)?;
        if disk
            .as_ref()
            .is_some_and(|disk| disk.len() > MAX_BODY_BYTES)
        {
            plan.refused.push(Refusal {
                path,
                reason: format!("file is larger than {MAX_BODY_BYTES} bytes"),
            });
            continue;
        }
        let base = old_state.pages.get(&path);
        let mut current: Option<ApiPage> = None;
        let mut etag = None;
        let mut rendered = None;
        if on_server.contains(&path) {
            let hint = disk
                .as_ref()
                .and(base.and_then(|base| base.etag.as_deref()));
            let read = api
                .read_page(&run_args.workspace, &run_args.project, &path, hint)
                .await?;
            etag = read.etag;
            if let Some(page) = read.page {
                rendered = Some(sync::render_page(&page)?);
                current = Some(page);
            }
        }
        let server = match (&rendered, on_server.contains(&path)) {
            (Some(bytes), _) => ServerSide::Rendered(bytes.as_bytes()),
            (None, true) => ServerSide::NotModified,
            (None, false) => ServerSide::Absent,
        };
        match decide(disk.as_deref(), base, server, args.prefer) {
            Action::Unchanged => {
                if let Some(bytes) = &rendered {
                    plan.adopted.insert(
                        path.clone(),
                        PageState {
                            hash: state::sha256_hex(bytes.as_bytes()),
                            etag,
                        },
                    );
                }
                plan.unchanged.push(path);
            }
            Action::Export { create } => {
                let Some(bytes) = rendered else {
                    bail!("server returned 304 for {path} with no local file to compare");
                };
                plan.exports.push(Export {
                    path,
                    create,
                    bytes,
                    etag,
                });
            }
            Action::Import { create } => {
                let disk = disk.unwrap_or_default();
                if !create && current.is_none() {
                    // A 304 proved the server unchanged, but the import
                    // still needs the page's metadata keys and rendering.
                    let page = api
                        .read_page(&run_args.workspace, &run_args.project, &path, None)
                        .await?
                        .page
                        .with_context(|| {
                            format!("server returned 304 for {path} without an ETag")
                        })?;
                    current = Some(page);
                }
                plan_import(&mut plan, path, create, disk, current.as_ref())?;
            }
            Action::Conflict => {
                let summary =
                    sync::diff_summary(disk.as_deref(), rendered.as_deref().map(str::as_bytes));
                plan.refused.push(Refusal {
                    path,
                    reason: format!(
                        "changed in the repository and on the server since the last sync \
                         ({summary}); pass --prefer repo or --prefer server"
                    ),
                });
            }
            Action::DeletedOnServer => plan.notices.push((
                path,
                "deleted on the server; the file is kept (deletes are not synced yet)",
            )),
            Action::DeletedInRepo => plan.notices.push((
                path,
                "deleted in the repository; the server page is kept (deletes are not synced yet)",
            )),
            Action::GoneOnBothSides => plan.forget.push(path),
        }
    }
    Ok(plan)
}

fn plan_import(
    plan: &mut Plan,
    path: String,
    create: bool,
    disk: Vec<u8>,
    current: Option<&ApiPage>,
) -> Result<()> {
    let (meta, body) = match page_file::parse(&disk) {
        Ok(parsed) => parsed,
        Err(error) => {
            plan.refused.push(Refusal {
                path,
                reason: format!("cannot import: {error:#}"),
            });
            return Ok(());
        }
    };
    let expected_server = match current {
        Some(page) => {
            let lost = keys_an_import_would_clear(&page.frontmatter);
            if !lost.is_empty() {
                plan.refused.push(Refusal {
                    path,
                    reason: format!(
                        "cannot import: the server page carries metadata a write would clear \
                         ({}); edit it through ai-memory instead",
                        lost.join(", ")
                    ),
                });
                return Ok(());
            }
            Some(state::sha256_hex(sync::render_page(page)?.as_bytes()))
        }
        None => None,
    };
    plan.imports.push(Import {
        path,
        create,
        meta,
        body,
        disk,
        expected_server,
    });
    Ok(())
}

/// Every `.md` file under the allowlisted families. A symlink or a path the
/// server could not hold is refused, never followed or guessed at.
fn local_pages(
    dest_root: &Path,
    families: &[String],
    refused: &mut Vec<Refusal>,
) -> Result<Vec<String>> {
    let mut found = Vec::new();
    for family in families {
        let mut pending = vec![(dest_root.join(family), family.clone())];
        while let Some((dir, rel)) = pending.pop() {
            let meta = match fs::symlink_metadata(&dir) {
                Ok(meta) => meta,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(anyhow::Error::new(e))
                        .context(format!("cannot inspect {}", dir.display()));
                }
            };
            if meta.file_type().is_symlink() {
                refused.push(Refusal {
                    path: rel,
                    reason: "is a symlink; wikisync never follows one".to_string(),
                });
                continue;
            }
            if !meta.is_dir() {
                continue;
            }
            for entry in
                fs::read_dir(&dir).with_context(|| format!("cannot read {}", dir.display()))?
            {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                // Hidden entries include this tool's own tmp files.
                if name.starts_with('.') {
                    continue;
                }
                let child_rel = format!("{rel}/{name}");
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    refused.push(Refusal {
                        path: child_rel,
                        reason: "is a symlink; wikisync never follows one".to_string(),
                    });
                } else if kind.is_dir() {
                    pending.push((entry.path(), child_rel));
                } else if name.ends_with(".md") {
                    match paths::validate_page_path(&child_rel) {
                        Ok(()) => found.push(child_rel),
                        Err(error) => refused.push(Refusal {
                            path: child_rel,
                            reason: format!("cannot import: {error:#}"),
                        }),
                    }
                }
                if found.len() > MAX_PAGES {
                    bail!("more than {MAX_PAGES} local pages; narrow --include");
                }
            }
        }
    }
    Ok(found)
}

/// Perform the imports, then the exports, then save the state once. The
/// state is also saved when an import fails partway, so the pages already
/// written are not re-classified as conflicts on the next run.
async fn apply_plan(
    api: &ApiClient,
    mcp: &McpClient,
    args: &RunArgs,
    dest_root: &Path,
    old_state: &SyncState,
    plan: Plan,
) -> Result<bool> {
    let mut new_state = old_state.clone();
    new_state.pages.extend(plan.adopted);
    for path in &plan.forget {
        new_state.pages.remove(path);
    }
    let mut wrote_files = false;
    for import in &plan.imports {
        match import_one(api, mcp, args, dest_root, import).await {
            Ok((entry, normalised)) => {
                wrote_files |= normalised;
                new_state.pages.insert(import.path.clone(), entry);
            }
            Err(error) => {
                state::save(dest_root, &new_state)?;
                return Err(error.context(format!(
                    "import of {} failed; the pages imported before it are recorded",
                    import.path
                )));
            }
        }
    }
    for export in &plan.exports {
        sync::atomic_write(dest_root, &export.path, export.bytes.as_bytes())?;
        wrote_files = true;
        new_state.pages.insert(
            export.path.clone(),
            PageState {
                hash: state::sha256_hex(export.bytes.as_bytes()),
                etag: export.etag.clone(),
            },
        );
    }
    state::save(dest_root, &new_state)?;
    Ok(wrote_files)
}

/// Re-check, write, read back. Returns the new state entry and whether the
/// file was rewritten because the server stored something other than the
/// file's bytes (the sanitizer redacting a secret, for example).
async fn import_one(
    api: &ApiClient,
    mcp: &McpClient,
    args: &RunArgs,
    dest_root: &Path,
    import: &Import,
) -> Result<(PageState, bool)> {
    let now = api
        .read_page_if_exists(&args.workspace, &args.project, &import.path)
        .await?;
    match (&now, &import.expected_server) {
        (Some(_), None) => bail!("{} was created on the server during this run", import.path),
        (None, Some(_)) => bail!("{} was deleted on the server during this run", import.path),
        (Some(page), Some(expected)) => {
            if state::sha256_hex(sync::render_page(page)?.as_bytes()) != *expected {
                bail!("{} changed on the server during this run", import.path);
            }
            let lost = keys_an_import_would_clear(&page.frontmatter);
            if !lost.is_empty() {
                bail!(
                    "{} gained metadata a write would clear during this run ({})",
                    import.path,
                    lost.join(", ")
                );
            }
        }
        (None, None) => {}
    }
    mcp.write_page(
        &args.workspace,
        &args.project,
        &import.path,
        &import.meta,
        &import.body,
    )
    .await?;
    let read = api
        .read_page(&args.workspace, &args.project, &import.path, None)
        .await?;
    let page = read.page.with_context(|| {
        format!(
            "server returned no body for {} after the write",
            import.path
        )
    })?;
    let rendered = sync::render_page(&page)?;
    let normalised = rendered.as_bytes() != import.disk.as_slice();
    if normalised {
        sync::atomic_write(dest_root, &import.path, rendered.as_bytes())?;
    }
    Ok((
        PageState {
            hash: state::sha256_hex(rendered.as_bytes()),
            etag: read.etag,
        },
        normalised,
    ))
}

fn print_plan(args: &RunArgs, families: &[String], plan: &Plan, apply: bool) {
    println!(
        "ai-memory-wikisync sync: {}/{} <-> {}",
        args.workspace,
        args.project,
        args.dest.display()
    );
    println!("  allowlist: {}", families.join(", "));
    for import in &plan.imports {
        let kind = if import.create { "create" } else { "update" };
        println!("  {:<9} {} (repo -> server, {kind})", "import", import.path);
    }
    for export in &plan.exports {
        let kind = if export.create { "create" } else { "update" };
        println!("  {:<9} {} (server -> repo, {kind})", "export", export.path);
    }
    for path in &plan.unchanged {
        println!("  {:<9} {path}", "unchanged");
    }
    for (path, notice) in &plan.notices {
        println!("  {:<9} {path} — {notice}", "notice");
    }
    for refusal in &plan.refused {
        println!("  {:<9} {} — {}", "REFUSED", refusal.path, refusal.reason);
    }
    println!(
        "  sync: {} import, {} export, {} unchanged, {} notice, {} refused",
        plan.imports.len(),
        plan.exports.len(),
        plan.unchanged.len(),
        plan.notices.len(),
        plan.refused.len()
    );
    if !apply {
        println!("  dry run: nothing was written (pass --apply to sync)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base(bytes: &[u8]) -> PageState {
        PageState {
            hash: state::sha256_hex(bytes),
            etag: Some("\"e\"".to_string()),
        }
    }

    #[test]
    fn decide_three_way_matrix() {
        let old = b"old".as_slice();
        let repo = b"repo edit".as_slice();
        let server = b"server edit".as_slice();
        let rendered = ServerSide::Rendered;

        // One side changed: the other side follows.
        assert_eq!(
            decide(Some(old), Some(&base(old)), rendered(server), None),
            Action::Export { create: false }
        );
        assert_eq!(
            decide(Some(repo), Some(&base(old)), rendered(old), None),
            Action::Import { create: false }
        );
        assert_eq!(
            decide(Some(repo), Some(&base(old)), ServerSide::NotModified, None),
            Action::Import { create: false }
        );
        assert_eq!(
            decide(Some(old), Some(&base(old)), ServerSide::NotModified, None),
            Action::Unchanged
        );

        // Both changed: a conflict, unless --prefer picks a side.
        assert_eq!(
            decide(Some(repo), Some(&base(old)), rendered(server), None),
            Action::Conflict
        );
        assert_eq!(
            decide(
                Some(repo),
                Some(&base(old)),
                rendered(server),
                Some(Prefer::Repo)
            ),
            Action::Import { create: false }
        );
        assert_eq!(
            decide(
                Some(repo),
                Some(&base(old)),
                rendered(server),
                Some(Prefer::Server)
            ),
            Action::Export { create: false }
        );
        // Both changed to the same bytes: nothing to do.
        assert_eq!(
            decide(Some(server), Some(&base(old)), rendered(server), None),
            Action::Unchanged
        );

        // A file with no base that differs from the server is a conflict
        // too: nothing says which side is newer.
        assert_eq!(
            decide(Some(repo), None, rendered(server), None),
            Action::Conflict
        );
        assert_eq!(
            decide(Some(server), None, rendered(server), None),
            Action::Unchanged
        );

        // Creates come from whichever side has the page.
        assert_eq!(
            decide(None, None, rendered(server), None),
            Action::Export { create: true }
        );
        assert_eq!(
            decide(Some(repo), None, ServerSide::Absent, None),
            Action::Import { create: true }
        );

        // Deletes are reported, never propagated, whatever --prefer says.
        for prefer in [None, Some(Prefer::Repo), Some(Prefer::Server)] {
            assert_eq!(
                decide(Some(old), Some(&base(old)), ServerSide::Absent, prefer),
                Action::DeletedOnServer
            );
            assert_eq!(
                decide(None, Some(&base(old)), rendered(server), prefer),
                Action::DeletedInRepo
            );
            assert_eq!(
                decide(None, Some(&base(old)), ServerSide::NotModified, prefer),
                Action::DeletedInRepo
            );
            assert_eq!(
                decide(None, Some(&base(old)), ServerSide::Absent, prefer),
                Action::GoneOnBothSides
            );
        }
    }

    #[test]
    fn metadata_an_import_would_clear_is_named() {
        let written_by_mcp = json!({
            "title": "T", "tier": "semantic", "pinned": true, "tags": ["a"],
            "type": "Rule", "generated": {"by": "x"}, "last_modified_by": "u"
        });
        assert!(keys_an_import_would_clear(&written_by_mcp).is_empty());
        let consolidated = json!({"title": "T", "summary": "s", "sources": [], "kind": "k"});
        assert_eq!(
            keys_an_import_would_clear(&consolidated),
            vec!["kind", "sources", "summary"]
        );
        assert!(keys_an_import_would_clear(&json!(null)).is_empty());
    }
}
