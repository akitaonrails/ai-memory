//! Two-way sync (#986, slices 3 and 4): repository edits flow back to the
//! server through the public `memory_write_page` and `memory_delete_page`
//! tools, server edits flow out as in `export`.
//!
//! Every page in an allowlisted family is classified from three versions:
//! the repository file, the state entry (what the last sync wrote), and the
//! server page rendered into file bytes. A change on one side is applied to
//! the other; a change on both sides is a conflict that only `--prefer`
//! resolves. Deletes are reported unless `--propagate-deletes` asks for them,
//! and even then only a side unchanged since the last sync is deleted.
//!
//! Nothing is written unless `--apply` is passed and nothing in the plan is
//! refused. Every server write and delete carries the version the plan was
//! classified against (`expected_page_id`, or `create_only` for a new page),
//! so a page that changes between classification and write is refused by the
//! server, reported, and left for the next run.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::client::{ApiClient, ApiPage};
use crate::mcp::{self, McpClient, Precondition, PreconditionFailed, Scope};
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

/// How `decide` treats conflicts and deletes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// Which side wins a page changed on both sides.
    pub prefer: Option<Prefer>,
    /// Propagate a delete instead of only reporting it.
    pub propagate_deletes: bool,
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
    /// Changed on both sides (or deleted on one, changed on the other), and
    /// no `--prefer`.
    Conflict,
    /// Delete the server page: the repository deleted the file.
    DeleteOnServer,
    /// Delete the repository file: the server deleted the page.
    DeleteInRepo,
    /// The server no longer has a page the last sync saw; reported only.
    DeletedOnServer,
    /// The repository no longer has a file the last sync wrote; reported only.
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
    policy: Policy,
) -> Action {
    let disk_hash = disk.map(state::sha256_hex);
    let disk_is_base = matches!((&disk_hash, base), (Some(disk), Some(base)) if *disk == base.hash);
    let prefer = policy.prefer;
    match server {
        ServerSide::Absent => match (disk.is_some(), base.is_some()) {
            (true, false) => Action::Import { create: true },
            (true, true) if !policy.propagate_deletes => Action::DeletedOnServer,
            (true, true) if disk_is_base => Action::DeleteInRepo,
            (true, true) => match prefer {
                Some(Prefer::Repo) => Action::Import { create: true },
                Some(Prefer::Server) => Action::DeleteInRepo,
                None => Action::Conflict,
            },
            (false, true) => Action::GoneOnBothSides,
            (false, false) => Action::Unchanged,
        },
        // A 304 proves the server unchanged since the base.
        ServerSide::NotModified => match (disk.is_some(), base.is_some()) {
            (false, true) if policy.propagate_deletes => Action::DeleteOnServer,
            (false, true) => Action::DeletedInRepo,
            (false, false) => Action::Unchanged,
            (true, _) if disk_is_base => Action::Unchanged,
            (true, _) => Action::Import { create: false },
        },
        ServerSide::Rendered(server) => {
            let server_hash = state::sha256_hex(server);
            let server_is_base = base.is_some_and(|base| base.hash == server_hash);
            match disk_hash {
                None if base.is_none() => Action::Export { create: true },
                None if !policy.propagate_deletes => Action::DeletedInRepo,
                None if server_is_base => Action::DeleteOnServer,
                None => match prefer {
                    Some(Prefer::Repo) => Action::DeleteOnServer,
                    Some(Prefer::Server) => Action::Export { create: true },
                    None => Action::Conflict,
                },
                Some(disk) if disk == server_hash => Action::Unchanged,
                Some(_) if disk_is_base => Action::Export { create: false },
                Some(_) if server_is_base => Action::Import { create: false },
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
    /// Propagate deletes instead of reporting them.
    pub propagate_deletes: bool,
    /// Ceiling on deletes in one run; more refuses the whole batch.
    pub max_deletes: usize,
}

/// Default `--max-deletes`.
pub const DEFAULT_MAX_DELETES: usize = 10;

/// What `sync` does with its plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Print the plan, write nothing.
    DryRun,
    /// Print the plan and perform it.
    Apply,
    /// Print the plan, write nothing, and report whether anything is pending
    /// (`--check`). A clone without a state file compares the repository with
    /// the server directly.
    Check,
}

/// Whether the repository and the server agree, as `--check` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckStatus {
    InSync,
    /// Imports, exports, deletes or unpropagated deletes are pending.
    Drift,
    /// Conflicts or refusals: a person has to decide.
    Blocked,
}

/// The plan's verdict, returned by every mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// Pages that differ between the two sides.
    pub pending: usize,
    /// Anything refused (a conflict, an invalid file, the delete ceiling).
    pub blocked: bool,
}

impl Outcome {
    pub fn status(self) -> CheckStatus {
        if self.blocked {
            CheckStatus::Blocked
        } else if self.pending > 0 {
            CheckStatus::Drift
        } else {
            CheckStatus::InSync
        }
    }
}

struct Import {
    path: String,
    create: bool,
    meta: PageMeta,
    body: String,
    disk: Vec<u8>,
    /// Server version the plan was classified against, sent as the write's
    /// precondition; `None` for a create, which is sent as `create_only`.
    expected_page_id: Option<String>,
}

struct Export {
    path: String,
    create: bool,
    bytes: String,
    etag: Option<String>,
    page_id: Option<String>,
}

/// A server page to delete because its file was deleted.
struct ServerDelete {
    path: String,
    /// `None` only from a server that predates version ids.
    expected_page_id: Option<String>,
}

/// A repository file to delete because its page was deleted.
struct FileDelete {
    path: String,
    /// The file must still hash to this right before the delete.
    expected_hash: String,
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
    server_deletes: Vec<ServerDelete>,
    file_deletes: Vec<FileDelete>,
    unchanged: Vec<String>,
    refused: Vec<Refusal>,
    /// A refusal of the whole batch (the delete ceiling).
    batch_refusal: Option<String>,
    notices: Vec<(String, &'static str)>,
    /// Pages that differ in a `--check` without state, where no base says
    /// which side changed.
    differs: Vec<String>,
    /// State to keep for pages this run leaves alone or adopts.
    adopted: BTreeMap<String, PageState>,
    forget: Vec<String>,
}

impl Plan {
    fn outcome(&self) -> Outcome {
        Outcome {
            pending: self.imports.len()
                + self.exports.len()
                + self.server_deletes.len()
                + self.file_deletes.len()
                + self.notices.len()
                + self.differs.len(),
            blocked: !self.refused.is_empty() || self.batch_refusal.is_some(),
        }
    }

    fn deletes(&self) -> usize {
        self.server_deletes.len() + self.file_deletes.len()
    }
}

/// What applying a plan did besides succeeding.
#[derive(Default)]
struct Applied {
    wrote_files: bool,
    deleted_files: Vec<String>,
    /// Pages left alone because they changed after classification.
    raced: Vec<String>,
}

/// Entry point behind `sync`.
pub async fn run(args: &SyncArgs, mode: SyncMode) -> Result<Outcome> {
    let run_args = &args.run;
    let families = sync::validate_allowlist(&run_args.include)?;
    let fs_mode = if mode == SyncMode::Apply {
        Mode::Apply
    } else {
        Mode::DryRun
    };
    let dest_root = sync::resolve_dest(&run_args.dest, fs_mode)?;
    let stateless_check = mode == SyncMode::Check && !state::state_path(&dest_root).exists();
    let old_state = state::load(&dest_root)?;
    let api = ApiClient::new(&run_args.server, run_args.token.clone())?;

    let plan = build_plan(
        &api,
        args,
        &families,
        &dest_root,
        &old_state,
        stateless_check,
    )
    .await?;
    print_plan(run_args, &families, &plan, mode);
    let outcome = plan.outcome();

    if mode != SyncMode::Apply {
        return Ok(outcome);
    }
    if let Some(reason) = &plan.batch_refusal {
        bail!("{reason}");
    }
    if !plan.refused.is_empty() {
        bail!(
            "{} page(s) refused; nothing was written. Resolve them (or pass --prefer for \
             conflicts) and run again",
            plan.refused.len()
        );
    }
    let mcp = McpClient::new(&run_args.server, run_args.token.clone())?;
    if !plan.imports.is_empty() || !plan.server_deletes.is_empty() {
        ensure_conditional_writes(&mcp, &plan).await?;
    }
    let applied = apply_plan(&api, &mcp, run_args, &dest_root, &old_state, plan).await?;
    if applied.wrote_files {
        sync::print_git_hints(run_args);
    }
    if !applied.deleted_files.is_empty() {
        print_git_rm_hint(run_args, &applied.deleted_files);
    }
    if !applied.raced.is_empty() {
        bail!(
            "{} page(s) changed during this run and were left for the next sync: {}",
            applied.raced.len(),
            applied.raced.join(", ")
        );
    }
    Ok(outcome)
}

/// Refuse to write through a server that would ignore the preconditions:
/// one without `expected_page_id` in its tool schema, or whose `/api/v1`
/// pages carry no version id to send.
async fn ensure_conditional_writes(mcp: &McpClient, plan: &Plan) -> Result<()> {
    let missing_id = plan
        .imports
        .iter()
        .any(|import| !import.create && import.expected_page_id.is_none())
        || plan
            .server_deletes
            .iter()
            .any(|delete| delete.expected_page_id.is_none());
    if missing_id {
        bail!("{}; nothing was written", mcp::server_too_old());
    }
    if let Err(error) = mcp.ensure_conditional_writes().await {
        bail!("{error:#}; nothing was written");
    }
    Ok(())
}

async fn build_plan(
    api: &ApiClient,
    args: &SyncArgs,
    families: &[String],
    dest_root: &Path,
    old_state: &SyncState,
    stateless_check: bool,
) -> Result<Plan> {
    let run_args = &args.run;
    let policy = Policy {
        prefer: args.prefer,
        propagate_deletes: args.propagate_deletes,
    };
    let mut plan = Plan::default();
    let listed = api
        .list_pages(&run_args.workspace, &run_args.project)
        .await?;
    let on_server: BTreeSet<String> = sync::select_pages(listed, families)?
        .into_iter()
        .map(|page| page.path)
        .collect();
    let in_repo = local_pages(dest_root, families, &mut plan.refused)?;
    // A symlink is refused once, by the walk, and never read or deleted.
    let refused_paths: BTreeSet<String> = plan
        .refused
        .iter()
        .map(|refusal| refusal.path.clone())
        .collect();
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
        .filter(|path| !refused_paths.contains(path))
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
        match decide(disk.as_deref(), base, server, policy) {
            Action::Unchanged => {
                if let Some(bytes) = &rendered {
                    plan.adopted.insert(
                        path.clone(),
                        PageState {
                            hash: state::sha256_hex(bytes.as_bytes()),
                            etag,
                            page_id: current.and_then(|page| page.id),
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
                    page_id: current.and_then(|page| page.id),
                });
            }
            Action::Import { create } => {
                let disk = disk.unwrap_or_default();
                if !create && current.is_none() {
                    // A 304 proved the server unchanged, but the import
                    // still needs the page's metadata keys and version id.
                    current = Some(full_page(api, run_args, &path).await?);
                }
                plan_import(&mut plan, path, create, disk, current.as_ref());
            }
            Action::Conflict if stateless_check && base.is_none() => plan.differs.push(path),
            Action::Conflict => {
                let reason = match (&disk, &rendered) {
                    (None, _) => "deleted in the repository and changed on the server since \
                                  the last sync"
                        .to_string(),
                    (Some(_), None) => "deleted on the server and changed in the repository \
                                        since the last sync"
                        .to_string(),
                    (Some(disk), Some(server)) => format!(
                        "changed in the repository and on the server since the last sync ({})",
                        sync::diff_summary(Some(disk), Some(server.as_bytes()))
                    ),
                };
                plan.refused.push(Refusal {
                    path,
                    reason: format!("{reason}; pass --prefer repo or --prefer server"),
                });
            }
            Action::DeleteOnServer => {
                let page = match current {
                    Some(page) => page,
                    None => full_page(api, run_args, &path).await?,
                };
                if page.pinned && args.prefer != Some(Prefer::Repo) {
                    plan.refused.push(Refusal {
                        path,
                        reason: "deleted in the repository, but the server page is pinned; \
                                 pass --prefer repo to delete it anyway"
                            .to_string(),
                    });
                } else {
                    plan.server_deletes.push(ServerDelete {
                        path,
                        expected_page_id: page.id,
                    });
                }
            }
            Action::DeleteInRepo => {
                let Some(disk) = disk else {
                    bail!("cannot delete {path}: the file is already gone");
                };
                plan.file_deletes.push(FileDelete {
                    path,
                    expected_hash: state::sha256_hex(&disk),
                });
            }
            Action::DeletedOnServer => plan.notices.push((
                path,
                "deleted on the server; the file is kept (pass --propagate-deletes to delete it)",
            )),
            Action::DeletedInRepo => plan.notices.push((
                path,
                "deleted in the repository; the server page is kept (pass --propagate-deletes \
                 to delete it)",
            )),
            Action::GoneOnBothSides => plan.forget.push(path),
        }
    }
    let deletes = plan.deletes();
    if deletes > args.max_deletes {
        plan.batch_refusal = Some(format!(
            "{deletes} deletes exceed --max-deletes {}; nothing was written. Check the deletes \
             above, then raise --max-deletes if they are intended",
            args.max_deletes
        ));
    }
    Ok(plan)
}

/// The full page, for a decision a `304` cannot settle.
async fn full_page(api: &ApiClient, args: &RunArgs, path: &str) -> Result<ApiPage> {
    api.read_page(&args.workspace, &args.project, path, None)
        .await?
        .page
        .with_context(|| format!("server returned 304 for {path} without an ETag"))
}

fn plan_import(
    plan: &mut Plan,
    path: String,
    create: bool,
    disk: Vec<u8>,
    current: Option<&ApiPage>,
) {
    let (meta, body) = match page_file::parse(&disk) {
        Ok(parsed) => parsed,
        Err(error) => {
            plan.refused.push(Refusal {
                path,
                reason: format!("cannot import: {error:#}"),
            });
            return;
        }
    };
    if let Some(page) = current {
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
            return;
        }
    }
    plan.imports.push(Import {
        path,
        create,
        meta,
        body,
        disk,
        expected_page_id: current.and_then(|page| page.id.clone()),
    });
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

/// Perform the imports, the exports, the server deletes, then the file
/// deletes, and save the state once. A page whose precondition no longer
/// holds is reported and skipped, keeping its old state entry; any other
/// failure stops the run, saving the state first so the pages already
/// written are not re-classified as conflicts on the next run.
async fn apply_plan(
    api: &ApiClient,
    mcp: &McpClient,
    args: &RunArgs,
    dest_root: &Path,
    old_state: &SyncState,
    plan: Plan,
) -> Result<Applied> {
    let mut new_state = old_state.clone();
    new_state.pages.extend(plan.adopted);
    for path in &plan.forget {
        new_state.pages.remove(path);
    }
    let mut applied = Applied::default();
    let scope = Scope {
        workspace: &args.workspace,
        project: &args.project,
    };
    for import in &plan.imports {
        match import_one(api, mcp, &scope, dest_root, import).await {
            Ok((entry, normalised)) => {
                applied.wrote_files |= normalised;
                new_state.pages.insert(import.path.clone(), entry);
            }
            Err(error) if error.is::<PreconditionFailed>() => {
                report_race(&import.path, &error);
                applied.raced.push(import.path.clone());
            }
            Err(error) => {
                state::save(dest_root, &new_state)?;
                return Err(error.context(format!(
                    "import of {} failed; the pages synced before it are recorded",
                    import.path
                )));
            }
        }
    }
    for export in &plan.exports {
        sync::atomic_write(dest_root, &export.path, export.bytes.as_bytes())?;
        applied.wrote_files = true;
        new_state.pages.insert(
            export.path.clone(),
            PageState {
                hash: state::sha256_hex(export.bytes.as_bytes()),
                etag: export.etag.clone(),
                page_id: export.page_id.clone(),
            },
        );
    }
    for delete in &plan.server_deletes {
        let result = match &delete.expected_page_id {
            Some(expected) => mcp.delete_page(&scope, &delete.path, expected).await,
            None => Err(anyhow::anyhow!(mcp::server_too_old())),
        };
        match result {
            Ok(()) => {
                new_state.pages.remove(&delete.path);
            }
            Err(error) if error.is::<PreconditionFailed>() => {
                report_race(&delete.path, &error);
                applied.raced.push(delete.path.clone());
            }
            Err(error) => {
                state::save(dest_root, &new_state)?;
                return Err(error.context(format!(
                    "delete of server page {} failed; the pages synced before it are recorded",
                    delete.path
                )));
            }
        }
    }
    for delete in &plan.file_deletes {
        match delete_file(dest_root, delete) {
            Ok(true) => {
                new_state.pages.remove(&delete.path);
                applied.deleted_files.push(delete.path.clone());
            }
            Ok(false) => {
                println!(
                    "  {:<9} {} — changed in the repository during this run; left for the next sync",
                    "CHANGED", delete.path
                );
                applied.raced.push(delete.path.clone());
            }
            Err(error) => {
                state::save(dest_root, &new_state)?;
                return Err(error.context(format!(
                    "delete of {} failed; the pages synced before it are recorded",
                    delete.path
                )));
            }
        }
    }
    state::save(dest_root, &new_state)?;
    Ok(applied)
}

fn report_race(path: &str, error: &anyhow::Error) {
    println!(
        "  {:<9} {path} — {error}; left for the next sync",
        "CHANGED"
    );
}

/// Delete one repository file through the same confinement as every write:
/// a validated path, no symlinked component, a regular file. The file must
/// still be the bytes the plan classified; returns `false` when it is not.
fn delete_file(dest_root: &Path, delete: &FileDelete) -> Result<bool> {
    let target = paths::secure_join(dest_root, &delete.path)?;
    let meta = fs::symlink_metadata(&target)
        .with_context(|| format!("cannot inspect {}", target.display()))?;
    if !meta.is_file() {
        bail!(
            "{} is not a regular file; refusing to delete it",
            target.display()
        );
    }
    let bytes = fs::read(&target).with_context(|| format!("cannot read {}", target.display()))?;
    if state::sha256_hex(&bytes) != delete.expected_hash {
        return Ok(false);
    }
    fs::remove_file(&target).with_context(|| format!("cannot delete {}", target.display()))?;
    Ok(true)
}

/// Write, then read back. Returns the new state entry and whether the file
/// was rewritten because the server stored something other than the file's
/// bytes (the sanitizer redacting a secret, for example).
async fn import_one(
    api: &ApiClient,
    mcp: &McpClient,
    scope: &Scope<'_>,
    dest_root: &Path,
    import: &Import,
) -> Result<(PageState, bool)> {
    let precondition = match (&import.expected_page_id, import.create) {
        (_, true) => Precondition::Absent,
        (Some(expected), false) => Precondition::Latest(expected),
        (None, false) => bail!(mcp::server_too_old()),
    };
    let written = mcp
        .write_page(
            scope,
            &import.path,
            &import.meta,
            &import.body,
            precondition,
        )
        .await?;
    let read = api
        .read_page(scope.workspace, scope.project, &import.path, None)
        .await?;
    let page = read.page.with_context(|| {
        format!(
            "server returned no body for {} after the write",
            import.path
        )
    })?;
    if written.is_some() && page.id != written {
        // Another write landed between ours and this read. Record the file
        // as what this run wrote, so the next run exports the newer version
        // instead of mistaking it for this import's rendering.
        return Ok((
            PageState {
                hash: state::sha256_hex(&import.disk),
                etag: None,
                page_id: written,
            },
            false,
        ));
    }
    let rendered = sync::render_page(&page)?;
    let normalised = rendered.as_bytes() != import.disk.as_slice();
    if normalised {
        sync::atomic_write(dest_root, &import.path, rendered.as_bytes())?;
    }
    Ok((
        PageState {
            hash: state::sha256_hex(rendered.as_bytes()),
            etag: read.etag,
            page_id: page.id.or(written),
        },
        normalised,
    ))
}

fn print_git_rm_hint(args: &RunArgs, deleted: &[String]) {
    let dest = args.dest.display();
    let files: Vec<String> = deleted
        .iter()
        .map(|path| format!("{dest}/{path}"))
        .collect();
    println!(
        "this tool never runs git; to commit the deletes you may run:\
         \n  git rm --quiet --ignore-unmatch -- {}",
        files.join(" ")
    );
}

fn print_plan(args: &RunArgs, families: &[String], plan: &Plan, mode: SyncMode) {
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
    for delete in &plan.server_deletes {
        println!(
            "  {:<9} {} (server page; deleted in the repository)",
            "delete", delete.path
        );
    }
    for delete in &plan.file_deletes {
        println!(
            "  {:<9} {} (repository file; deleted on the server)",
            "delete", delete.path
        );
    }
    for path in &plan.unchanged {
        println!("  {:<9} {path}", "unchanged");
    }
    for path in &plan.differs {
        println!(
            "  {:<9} {path} (no sync state to say which side changed)",
            "differs"
        );
    }
    for (path, notice) in &plan.notices {
        println!("  {:<9} {path} — {notice}", "notice");
    }
    for refusal in &plan.refused {
        println!("  {:<9} {} — {}", "REFUSED", refusal.path, refusal.reason);
    }
    if let Some(reason) = &plan.batch_refusal {
        println!("  {:<9} {reason}", "REFUSED");
    }
    println!(
        "  sync: {} import, {} export, {} delete, {} unchanged, {} notice, {} refused",
        plan.imports.len(),
        plan.exports.len(),
        plan.deletes(),
        plan.unchanged.len(),
        plan.notices.len(),
        plan.refused.len() + usize::from(plan.batch_refusal.is_some())
    );
    match mode {
        SyncMode::DryRun => println!("  dry run: nothing was written (pass --apply to sync)"),
        SyncMode::Check => {
            let verdict = match plan.outcome().status() {
                CheckStatus::InSync => "in sync",
                CheckStatus::Drift => "drift: changes are pending",
                CheckStatus::Blocked => "blocked: conflicts or refusals need a decision",
            };
            println!("  check: {verdict} (nothing was written)");
        }
        SyncMode::Apply => {}
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
            page_id: Some("v1".to_string()),
        }
    }

    fn policy(prefer: Option<Prefer>, propagate_deletes: bool) -> Policy {
        Policy {
            prefer,
            propagate_deletes,
        }
    }

    const PREFERENCES: [Option<Prefer>; 3] = [None, Some(Prefer::Repo), Some(Prefer::Server)];

    #[test]
    fn decide_three_way_matrix() {
        let old = b"old".as_slice();
        let repo = b"repo edit".as_slice();
        let server = b"server edit".as_slice();
        let rendered = ServerSide::Rendered;
        let none = Policy::default();

        // One side changed: the other side follows.
        assert_eq!(
            decide(Some(old), Some(&base(old)), rendered(server), none),
            Action::Export { create: false }
        );
        assert_eq!(
            decide(Some(repo), Some(&base(old)), rendered(old), none),
            Action::Import { create: false }
        );
        assert_eq!(
            decide(Some(repo), Some(&base(old)), ServerSide::NotModified, none),
            Action::Import { create: false }
        );
        assert_eq!(
            decide(Some(old), Some(&base(old)), ServerSide::NotModified, none),
            Action::Unchanged
        );

        // Both changed: a conflict, unless --prefer picks a side.
        assert_eq!(
            decide(Some(repo), Some(&base(old)), rendered(server), none),
            Action::Conflict
        );
        assert_eq!(
            decide(
                Some(repo),
                Some(&base(old)),
                rendered(server),
                policy(Some(Prefer::Repo), false)
            ),
            Action::Import { create: false }
        );
        assert_eq!(
            decide(
                Some(repo),
                Some(&base(old)),
                rendered(server),
                policy(Some(Prefer::Server), false)
            ),
            Action::Export { create: false }
        );
        // Both changed to the same bytes: nothing to do.
        assert_eq!(
            decide(Some(server), Some(&base(old)), rendered(server), none),
            Action::Unchanged
        );

        // A file with no base that differs from the server is a conflict
        // too: nothing says which side is newer.
        assert_eq!(
            decide(Some(repo), None, rendered(server), none),
            Action::Conflict
        );
        assert_eq!(
            decide(Some(server), None, rendered(server), none),
            Action::Unchanged
        );

        // Creates come from whichever side has the page.
        assert_eq!(
            decide(None, None, rendered(server), none),
            Action::Export { create: true }
        );
        assert_eq!(
            decide(Some(repo), None, ServerSide::Absent, none),
            Action::Import { create: true }
        );
    }

    #[test]
    fn deletes_are_only_reported_without_the_flag() {
        let old = b"old".as_slice();
        let repo = b"repo edit".as_slice();
        let server = b"server edit".as_slice();
        for prefer in PREFERENCES {
            let off = policy(prefer, false);
            for disk in [old, repo] {
                assert_eq!(
                    decide(Some(disk), Some(&base(old)), ServerSide::Absent, off),
                    Action::DeletedOnServer
                );
            }
            for server in [old, server] {
                assert_eq!(
                    decide(None, Some(&base(old)), ServerSide::Rendered(server), off),
                    Action::DeletedInRepo
                );
            }
            assert_eq!(
                decide(None, Some(&base(old)), ServerSide::NotModified, off),
                Action::DeletedInRepo
            );
            assert_eq!(
                decide(None, Some(&base(old)), ServerSide::Absent, off),
                Action::GoneOnBothSides
            );
        }
    }

    #[test]
    fn deletes_propagate_only_from_an_unchanged_side() {
        let old = b"old".as_slice();
        let repo = b"repo edit".as_slice();
        let server = b"server edit".as_slice();
        for prefer in PREFERENCES {
            let on = policy(prefer, true);
            // The other side is unchanged since the last sync: delete it,
            // whatever --prefer says.
            assert_eq!(
                decide(None, Some(&base(old)), ServerSide::Rendered(old), on),
                Action::DeleteOnServer
            );
            assert_eq!(
                decide(None, Some(&base(old)), ServerSide::NotModified, on),
                Action::DeleteOnServer
            );
            assert_eq!(
                decide(Some(old), Some(&base(old)), ServerSide::Absent, on),
                Action::DeleteInRepo
            );
            assert_eq!(
                decide(None, Some(&base(old)), ServerSide::Absent, on),
                Action::GoneOnBothSides
            );
            // Never synced: nothing was deleted, the page is new.
            assert_eq!(
                decide(Some(repo), None, ServerSide::Absent, on),
                Action::Import { create: true }
            );
            assert_eq!(
                decide(None, None, ServerSide::Rendered(server), on),
                Action::Export { create: true }
            );
        }

        // Deleted on one side, changed on the other: a conflict until a
        // side is preferred.
        let repo_deleted = |prefer| {
            decide(
                None,
                Some(&base(old)),
                ServerSide::Rendered(server),
                policy(prefer, true),
            )
        };
        assert_eq!(repo_deleted(None), Action::Conflict);
        assert_eq!(repo_deleted(Some(Prefer::Repo)), Action::DeleteOnServer);
        assert_eq!(
            repo_deleted(Some(Prefer::Server)),
            Action::Export { create: true }
        );
        let server_deleted = |prefer| {
            decide(
                Some(repo),
                Some(&base(old)),
                ServerSide::Absent,
                policy(prefer, true),
            )
        };
        assert_eq!(server_deleted(None), Action::Conflict);
        assert_eq!(
            server_deleted(Some(Prefer::Repo)),
            Action::Import { create: true }
        );
        assert_eq!(server_deleted(Some(Prefer::Server)), Action::DeleteInRepo);
    }

    #[test]
    fn check_status_ranks_refusals_over_drift() {
        let outcome = |pending, blocked| Outcome { pending, blocked }.status();
        assert_eq!(outcome(0, false), CheckStatus::InSync);
        assert_eq!(outcome(2, false), CheckStatus::Drift);
        assert_eq!(outcome(0, true), CheckStatus::Blocked);
        assert_eq!(outcome(2, true), CheckStatus::Blocked);
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
