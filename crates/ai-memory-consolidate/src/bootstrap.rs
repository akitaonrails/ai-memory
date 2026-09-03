//! Pre-load an existing project's history into the wiki.
//!
//! The use case: a developer has been working on a project for
//! months. They install ai-memory today. The wiki is empty. The
//! first few sessions are net-zero (you're populating, not
//! retrieving) because none of the project's prior decisions /
//! gotchas / architecture notes are in ai-memory yet — even though
//! they're written down in `git log`, README, `docs/`, and module
//! doc-comments.
//!
//! `bootstrap` does a one-shot LLM-summarisation of those sources
//! into seed wiki pages so the project starts with warm context.
//! Pages are tagged `bootstrapped_at: <ts>` in their frontmatter so
//! a future lint pass can treat them as lower-confidence than
//! session-grown pages if needed.
//!
//! ## Cost model
//!
//! Input is capped via `max_input_tokens` (CLI default 150k). We
//! estimate token count locally at chars/4 and drop lower-priority
//! sources first when over budget. Large bundles are split into
//! sequential LLM chunks (`chunk_input_tokens`, default 24k) so
//! provider context limits are not exceeded.
//!
//! ## Idempotency
//!
//! First write produces `<wiki>/<workspace>/<project>/bootstrap.md`
//! recording the run (timestamp, source counts, generated page count).
//! Re-running refuses unless `--force` is passed. The user can always
//! delete the manifest (and the generated pages) to reset.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ai_memory_core::{PagePath, ProjectId, Tier, WorkspaceId};
use ai_memory_llm::{ChatMessage, ChatRequest, LlmProvider, Role, complete_structured};
use ai_memory_store::ReaderPool;
use ai_memory_wiki::{Wiki, WritePageRequest};
use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, info, warn};

/// Rough characters-per-token estimate used for budget enforcement.
/// 4 is the standard heuristic for English prose (cl100k, gpt-4
/// tokenizer family). Don't rely on it for billing math — it's
/// only used to decide which sources to drop.
const CHARS_PER_TOKEN: usize = 4;

/// Errors returned from [`Bootstrap::run`].
#[derive(Debug, Error)]
pub enum BootstrapError {
    /// Project repo path doesn't exist or isn't a git repo.
    #[error("repo path {0} is not a git repository")]
    NotARepo(PathBuf),
    /// The project already has a `bootstrap.md` manifest. Re-run with
    /// `--force` to overwrite.
    #[error(
        "this project is already bootstrapped (its bootstrap manifest already exists). \
         Pass --force to re-run."
    )]
    AlreadyBootstrapped,
    /// All source categories were excluded; nothing to do.
    #[error("no input sources selected; remove at least one --exclude-* flag")]
    NoSources,
    /// LLM call failed.
    #[error(transparent)]
    Llm(#[from] ai_memory_llm::LlmError),
    /// Wiki write failed.
    #[error(transparent)]
    Wiki(#[from] ai_memory_wiki::WikiError),
    /// Store read failed (used by the idempotency check).
    #[error(transparent)]
    Store(#[from] ai_memory_store::StoreError),
    /// IO error (reading docs, walking the repo, …).
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// libgit2 error from git_log() reading the commit history.
    #[error(transparent)]
    Git(#[from] git2::Error),
}

/// What kind of source we collected text from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// A line summary of one commit.
    GitCommit,
    /// The repo root's README.md.
    Readme,
    /// A file under `docs/`.
    DocFile,
    /// Module-level `//!` doc-comments at the top of a `.rs` file.
    ModuleHeader,
    /// CLAUDE.md / AGENTS.md / similar project-rules file.
    ProjectRules,
}

impl SourceKind {
    /// Drop priority — sources with higher numeric priority are
    /// dropped FIRST when over budget. (We keep the most valuable
    /// inputs and shed the noise.)
    #[must_use]
    pub const fn drop_priority(self) -> u8 {
        match self {
            // Project rules are usually small but very high-signal —
            // always keep.
            Self::ProjectRules => 0,
            // README is the single most useful project doc.
            Self::Readme => 1,
            // docs/ second.
            Self::DocFile => 2,
            // Recent git commits keep.
            Self::GitCommit => 3,
            // Module headers are nice-to-have; first to drop.
            Self::ModuleHeader => 4,
        }
    }
}

/// One unit of source text fed to the LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapSource {
    /// Origin (used for prioritisation when over budget).
    pub kind: SourceKind,
    /// A short label, included in the LLM prompt to help the model
    /// distinguish sources. Examples: "git: feat: …", "README".
    pub label: String,
    /// The full text content fed to the LLM.
    pub text: String,
}

impl BootstrapSource {
    /// Estimated token count via chars/4.
    #[must_use]
    pub fn estimated_tokens(&self) -> usize {
        (self.label.len() + self.text.len() + 16).div_ceil(CHARS_PER_TOKEN)
    }
}

/// LLM-produced page describing one bootstrap output.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BootstrapPage {
    /// Relative wiki path. Use the conventions:
    /// - `concepts/<slug>.md` for evergreen architectural notes
    /// - `decisions/0001-<slug>.md` for ADR-shaped commits
    /// - `gotchas/<slug>.md` for failure-mode notes
    pub path: String,
    /// Page title (renders as H1).
    pub title: String,
    /// Markdown body. Don't include the frontmatter — we add it.
    pub body_markdown: String,
    /// Up to ~5 short tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

/// LLM structured output: a batch of bootstrap pages.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BootstrapBatch {
    /// Pages to create.
    ///
    /// Defaulted because a chunk legitimately has nothing to add: later
    /// chunks receive the paths earlier ones already wrote, so a model
    /// that judges the material covered answers with a rationale and no
    /// `pages` key. Anthropic's `tool_use` schema does not enforce
    /// required fields the way OpenAI's `strict: true` does, so that
    /// answer reaches serde — and without a default it aborts the whole
    /// multi-chunk run, discarding every page the earlier chunks paid for.
    #[serde(default)]
    pub pages: Vec<BootstrapPage>,
    /// Brief one-paragraph note about what was processed, surfaced
    /// in the wiki/bootstrap.md manifest + the auto-commit message.
    #[serde(default)]
    pub rationale: String,
}

/// Per-kind breakdown of what was actually sent to the LLM. Lets the
/// CLI surface "we loaded 23 commits + the README + 8 docs" rather
/// than a single opaque sources-count, so the user can calibrate
/// what ai-memory actually saw vs. didn't.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourceCounts {
    /// Number of git-commit summaries kept.
    pub git_commits: usize,
    /// 1 if the README was kept, else 0.
    pub readme: usize,
    /// Number of `docs/**/*.md` files kept.
    pub doc_files: usize,
    /// Number of Rust `//!` module headers kept.
    pub module_headers: usize,
    /// Number of CLAUDE.md / AGENTS.md style rule files kept.
    pub project_rules: usize,
}

impl SourceCounts {
    /// Tally from the post-prune source list.
    #[must_use]
    pub fn from_sources(sources: &[BootstrapSource]) -> Self {
        let mut c = Self::default();
        for s in sources {
            match s.kind {
                SourceKind::GitCommit => c.git_commits += 1,
                SourceKind::Readme => c.readme += 1,
                SourceKind::DocFile => c.doc_files += 1,
                SourceKind::ModuleHeader => c.module_headers += 1,
                SourceKind::ProjectRules => c.project_rules += 1,
            }
        }
        c
    }
}

/// Outcome reported back to the CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootstrapOutcome {
    /// Number of sources collected (before any budget pruning).
    pub sources_collected: usize,
    /// Number of sources actually sent to the LLM (after pruning).
    pub sources_sent: usize,
    /// Number of sources dropped to stay under `max_input_tokens`.
    pub sources_dropped: usize,
    /// Per-kind breakdown of what was sent to the LLM.
    pub sources_by_kind: SourceCounts,
    /// Token budget used by the chosen sources (best-effort estimate).
    pub estimated_input_tokens: usize,
    /// Pages written to the wiki (empty if dry_run).
    pub pages_written: Vec<String>,
    /// One-paragraph LLM-authored summary, mirrored into the manifest.
    pub rationale: String,
    /// True when the operation skipped the LLM call entirely.
    pub dry_run: bool,
    /// Number of LLM calls made (1 when chunking is off or unnecessary).
    #[serde(default)]
    pub llm_chunks: usize,
}

/// Bootstrap configuration. Built by the CLI from `BootstrapArgs`
/// after resolving auto-detect defaults (repo path, workspace, etc.)
/// to concrete values.
#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    /// Project repo root on the client's filesystem. Used by
    /// [`Bootstrap::run`]'s client-side [`collect_sources`] call.
    /// Ignored by [`Bootstrap::process_sources`] — sources arrive
    /// pre-collected on the server-side path.
    pub repo_path: PathBuf,
    /// Workspace identifier the generated pages belong to.
    pub workspace_id: WorkspaceId,
    /// Project identifier the generated pages belong to.
    pub project_id: ProjectId,
    /// Token budget for LLM input; lower-priority sources are
    /// dropped first when over budget.
    pub max_input_tokens: usize,
    /// Max estimated input tokens per LLM call. When the pruned
    /// bundle exceeds this, sources are split into sequential chunks
    /// (each call stays within provider context). `0` disables chunking.
    pub chunk_input_tokens: usize,
    /// Original source count before client-side prune (when the CLI
    /// pre-trims the POST body). `None` → use the incoming vec length.
    pub sources_collected: Option<usize>,
    /// Include git-commit history.
    pub include_git: bool,
    /// Include `README.md` at the repo root.
    pub include_readme: bool,
    /// Include `docs/**/*.md`.
    pub include_docs: bool,
    /// Include Rust `//!` module-level doc-comments.
    pub include_code: bool,
    /// git-log `--since` filter; supports "N days ago" / "N years
    /// ago" / YYYY-MM-DD.
    pub since: Option<String>,
    /// Collect + estimate + prompt, but DON'T call the LLM and
    /// DON'T write to the wiki. Useful for pre-flight verification.
    pub dry_run: bool,
    /// Allow re-running even when `wiki/bootstrap.md` already exists.
    pub force: bool,
}

/// Bootstrap driver. Holds the LLM provider + wiki handle + reader
/// pool needed to ingest sources, summarise them, and write the
/// generated pages.
pub struct Bootstrap {
    /// Reader pool used by the idempotency check.
    pub reader: ReaderPool,
    /// Wiki handle the generated pages are written through.
    pub wiki: Wiki,
    /// LLM provider used to summarise sources into pages.
    pub llm: Arc<dyn LlmProvider>,
}

impl Bootstrap {
    /// Process pre-collected sources end-to-end: prune to budget,
    /// call the LLM, write pages, return the outcome. Server-side
    /// entry point — does NOT collect from disk.
    ///
    /// # Errors
    /// Propagates [`BootstrapError`] for any of: LLM failure, wiki
    /// write failure, or already-bootstrapped (without `--force`).
    pub async fn process_sources(
        &self,
        cfg: &BootstrapConfig,
        sources: Vec<BootstrapSource>,
    ) -> Result<BootstrapOutcome, BootstrapError> {
        // ---- idempotency check ------------------------------------
        // Check whether bootstrap.md already exists on disk in the per-project
        // directory. If it parses cleanly, this project was bootstrapped before.
        if !cfg.force {
            let manifest_path =
                PagePath::new("bootstrap.md").expect("hard-coded manifest path is valid");
            if self
                .wiki
                .read_page(cfg.workspace_id, cfg.project_id, &manifest_path)
                .is_ok()
            {
                return Err(BootstrapError::AlreadyBootstrapped);
            }
        }

        if sources.is_empty() {
            return Err(BootstrapError::NoSources);
        }

        let incoming = sources.len();
        let (kept, _prune_internal, est_tokens) =
            prune_sources_to_budget(sources, cfg.max_input_tokens);
        if kept.is_empty() {
            return Err(BootstrapError::NoSources);
        }
        let (collected, sources_sent, sources_dropped) =
            bootstrap_source_counts(incoming, cfg.sources_collected, &kept);
        info!(
            collected,
            sources_sent,
            sources_dropped,
            est_tokens,
            "bootstrap sources prioritised + budget-capped",
        );

        let kept_counts = SourceCounts::from_sources(&kept);
        let chunk_budget = effective_chunk_budget(cfg.chunk_input_tokens, cfg.max_input_tokens);

        // ---- dry run early-exits before the LLM call --------------
        if cfg.dry_run {
            return Ok(BootstrapOutcome {
                sources_collected: collected,
                sources_sent,
                sources_dropped,
                sources_by_kind: kept_counts,
                estimated_input_tokens: est_tokens,
                pages_written: Vec::new(),
                rationale: "(dry-run; LLM not invoked)".to_string(),
                dry_run: true,
                llm_chunks: plan_bootstrap_chunks(kept.clone(), chunk_budget).len(),
            });
        }

        // ---- LLM call(s) — chunked when over chunk_input_tokens ----
        let chunks = plan_bootstrap_chunks(kept, chunk_budget);
        let llm_chunks = chunks.len();
        info!(llm_chunks, chunk_budget, "bootstrap LLM chunk plan",);

        let mut pages_by_path = std::collections::BTreeMap::new();
        let mut rationales = Vec::with_capacity(llm_chunks);

        for (idx, chunk) in chunks.iter().enumerate() {
            let mut prior: Vec<String> = pages_by_path.keys().cloned().collect();
            prior.sort_unstable();
            let prior_refs: Vec<&str> = prior.iter().map(String::as_str).collect();
            let request = build_chunk_request(chunk, idx + 1, llm_chunks, &prior_refs);
            info!(
                chunk = idx + 1,
                total = llm_chunks,
                sources = chunk.len(),
                est_tokens = chunk
                    .iter()
                    .map(BootstrapSource::estimated_tokens)
                    .sum::<usize>(),
                "bootstrap LLM chunk",
            );
            let batch: BootstrapBatch = complete_structured(&*self.llm, request).await?;
            rationales.push(batch.rationale);
            for page in batch.pages {
                insert_bootstrap_page(&mut pages_by_path, page, idx + 1);
            }
        }

        let merged_pages: Vec<BootstrapPage> = pages_by_path.into_values().collect();
        let rationale = if rationales.len() == 1 {
            rationales.pop().unwrap_or_default()
        } else {
            format!(
                "Processed in {llm_chunks} LLM chunks.\n\n{}",
                rationales.join("\n\n---\n\n")
            )
        };

        // ---- write pages ------------------------------------------
        let now = Timestamp::now();
        let mut requests = Vec::with_capacity(merged_pages.len() + 1);
        let mut written_paths = Vec::with_capacity(merged_pages.len() + 1);
        for page in &merged_pages {
            let path = match PagePath::new(&page.path) {
                Ok(p) => p,
                Err(e) => {
                    warn!(path = %page.path, error = %e, "skipping bootstrap page with invalid path");
                    continue;
                }
            };
            written_paths.push(page.path.clone());
            requests.push(WritePageRequest {
                workspace_id: cfg.workspace_id,
                project_id: cfg.project_id,
                path,
                frontmatter: build_frontmatter(&page.title, &page.tags, now),
                body: page.body_markdown.clone(),
                tier: Tier::Semantic,
                pinned: false,
                title: Some(page.title.clone()),
                admission_ctx: None,
                author_id: None,
                actor: ai_memory_core::ActorContext::anonymous(),
            });
        }
        // Plus the manifest itself.
        let manifest_body = render_manifest_body(ManifestRender {
            now,
            sources_collected: collected,
            sources_sent,
            sources_dropped,
            est_tokens,
            rationale: &rationale,
            pages: &written_paths,
            llm_chunks,
        });
        requests.push(WritePageRequest {
            workspace_id: cfg.workspace_id,
            project_id: cfg.project_id,
            path: PagePath::new("bootstrap.md").expect("static path"),
            frontmatter: serde_json::json!({
                "title": "Bootstrap manifest",
                "tier": "semantic",
                "bootstrapped_at": now.to_string(),
                "tags": ["bootstrap", "manifest"],
            }),
            body: manifest_body,
            tier: Tier::Semantic,
            pinned: true,
            title: Some("Bootstrap manifest".into()),
            admission_ctx: None,
            author_id: None,
            actor: ai_memory_core::ActorContext::anonymous(),
        });

        let _ids = self.wiki.apply_batch(requests).await?;
        let _ = self.wiki.commit_all(&format!(
            "bootstrap: {llm_chunks} LLM chunk(s), ingested {collected} sources, wrote {} pages",
            written_paths.len() + 1,
        ));

        // Manifest is the last entry but conceptually first; list it.
        let mut out_paths = written_paths.clone();
        out_paths.push("bootstrap.md".to_string());

        Ok(BootstrapOutcome {
            sources_collected: collected,
            sources_sent,
            sources_dropped,
            sources_by_kind: kept_counts,
            estimated_input_tokens: est_tokens,
            pages_written: out_paths,
            rationale,
            dry_run: false,
            llm_chunks,
        })
    }

    /// Convenience wrapper that collects sources from disk then runs
    /// [`Self::process_sources`]. Used by tests and any direct caller
    /// that has filesystem access.
    ///
    /// # Errors
    /// Propagates [`BootstrapError`] from either collection or processing.
    pub async fn run(&self, cfg: &BootstrapConfig) -> Result<BootstrapOutcome, BootstrapError> {
        let sources = collect_sources(
            &cfg.repo_path,
            cfg.since.as_deref(),
            cfg.include_git,
            cfg.include_readme,
            cfg.include_docs,
            cfg.include_code,
        )?;
        self.process_sources(cfg, sources).await
    }
}

// --------------------------------------------------------------------
// Source collection
// --------------------------------------------------------------------

/// Collect sources from a project repo on disk. IO-only — no LLM,
/// no store, no wiki. The CLI calls this before posting the bundle
/// to the server; the server's own bootstrap-route handler does NOT
/// call this (the server can't see the caller's filesystem).
///
/// # Errors
/// Returns [`BootstrapError`] when the repo path is invalid, files
/// can't be read, or the git history can't be walked.
pub fn collect_sources(
    repo_path: &std::path::Path,
    since: Option<&str>,
    include_git: bool,
    include_readme: bool,
    include_docs: bool,
    include_code: bool,
) -> Result<Vec<BootstrapSource>, BootstrapError> {
    let mut sources = Vec::<BootstrapSource>::new();
    if include_git {
        sources.extend(collect_git_commits(repo_path, since)?);
    }
    if include_readme {
        sources.extend(collect_readme(repo_path)?);
    }
    if include_docs {
        sources.extend(collect_docs(repo_path)?);
    }
    if include_code {
        sources.extend(collect_rust_module_headers(repo_path)?);
    }
    // Project-rules files (CLAUDE.md / AGENTS.md) are always collected —
    // they're the highest-signal input and very small.
    sources.extend(collect_project_rules(repo_path)?);
    Ok(sources)
}

/// Walk up from `start` looking for the nearest `.git` and return
/// the repo root path. Pure libgit2 (no `git` binary required), so
/// the slim runtime container can resolve repo roots too.
///
/// # Errors
/// Returns `BootstrapError::NotARepo` when no repository is found
/// at or above `start`.
pub fn discover_repo_root(start: &Path) -> Result<PathBuf, BootstrapError> {
    let repo = git2::Repository::discover(start)
        .map_err(|_| BootstrapError::NotARepo(start.to_path_buf()))?;
    // `path()` returns the .git dir; the workdir is the parent unless
    // it's a bare repo (which bootstrap doesn't support anyway).
    repo.workdir()
        .map(Path::to_path_buf)
        .ok_or_else(|| BootstrapError::NotARepo(start.to_path_buf()))
}

/// Like [`discover_repo_root`], but when `start` is inside a git
/// worktree the function follows the `.git` commondir pointer back to
/// the **main** repository root rather than returning the worktree's
/// own working directory.
///
/// This is the right choice when the caller needs a stable identity
/// for the repository (e.g. deriving a project name) — all worktrees
/// of the same repo should resolve to the same root.
///
/// # Errors
/// Returns `BootstrapError::NotARepo` when no repository is found
/// at or above `start`.
pub fn discover_main_repo_root(start: &Path) -> Result<PathBuf, BootstrapError> {
    let repo = match git2::Repository::discover(start) {
        Ok(repo) => repo,
        Err(_) => return discover_main_repo_root_fallback(start),
    };
    // Bare repos have no working directory; `commondir()` IS the repo root
    // (there is no `.git/` subdirectory), so `.parent()` would return the
    // grandparent — wrong.  Fall back to basename(cwd) via NotARepo so the
    // router handles it safely.
    if repo.is_bare() {
        return Err(BootstrapError::NotARepo(start.to_path_buf()));
    }
    // `commondir()` returns the shared .git directory.  For a regular
    // checkout it equals `path()` (i.e. `<repo>/.git/`); for a
    // worktree it points to the *main* repo's `.git/` — so its parent
    // is always the main repository root.
    repo.commondir()
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| BootstrapError::NotARepo(start.to_path_buf()))
}

#[cfg(windows)]
fn discover_main_repo_root_fallback(start: &Path) -> Result<PathBuf, BootstrapError> {
    let is_bare = std::process::Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--is-bare-repository"])
        .output()
        .map_err(|_| BootstrapError::NotARepo(start.to_path_buf()))?;
    if !is_bare.status.success() {
        return Err(BootstrapError::NotARepo(start.to_path_buf()));
    }
    let is_bare = String::from_utf8_lossy(&is_bare.stdout).trim() == "true";

    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .map_err(|_| BootstrapError::NotARepo(start.to_path_buf()))?;
    if !output.status.success() {
        return Err(BootstrapError::NotARepo(start.to_path_buf()));
    }
    let common_dir = String::from_utf8_lossy(&output.stdout);
    let common_dir = common_dir.trim();
    if common_dir.is_empty() {
        return Err(BootstrapError::NotARepo(start.to_path_buf()));
    }
    main_root_from_common_dir(Path::new(common_dir), is_bare, start)
}

#[cfg(not(windows))]
fn discover_main_repo_root_fallback(start: &Path) -> Result<PathBuf, BootstrapError> {
    Err(BootstrapError::NotARepo(start.to_path_buf()))
}

#[cfg(any(windows, test))]
fn main_root_from_common_dir(
    common_dir: &Path,
    is_bare: bool,
    start: &Path,
) -> Result<PathBuf, BootstrapError> {
    if is_bare {
        return Err(BootstrapError::NotARepo(start.to_path_buf()));
    }
    common_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| BootstrapError::NotARepo(start.to_path_buf()))
}

/// Strategy for [`derive_project_name`]: which path "wins" when a
/// `cwd` lives inside a git repo. Both the CLI's `resolve_project_name`
/// and the hook router's `resolve_project_ids` call through this
/// helper, so a single config-driven choice keeps the two surfaces
/// aligned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectNameStrategy {
    /// Take the basename of `cwd` directly. Worktrees and subdirs of
    /// a repo each become a different project.
    Basename,
    /// Walk up to the **main** repo root (via
    /// [`discover_main_repo_root`]) and take its basename so all
    /// worktrees / subdirs of the same repo collapse to a stable
    /// project identity. Falls back to [`ProjectNameStrategy::Basename`]
    /// semantics when no git repo is found at or above `cwd`.
    MainRepoRoot,
}

/// Derive the project name for a working-directory path under the
/// chosen [`ProjectNameStrategy`]. The optional second tuple value
/// is the canonical repo root when the strategy was
/// [`ProjectNameStrategy::MainRepoRoot`] and a git repo was found —
/// callers that want to register the resolved `repo_path` against
/// the project record use it; CLI callers ignore it.
///
/// `None` is returned only for degenerate paths that cannot be
/// reduced to a non-empty basename (e.g. `/`, `""`).
#[must_use]
pub fn derive_project_name(
    cwd: &Path,
    strategy: ProjectNameStrategy,
) -> Option<(String, Option<PathBuf>)> {
    if matches!(strategy, ProjectNameStrategy::MainRepoRoot)
        && let Ok(root) = discover_main_repo_root(cwd)
        && let Some(name) = basename_with_alt_separators(&root)
    {
        return Some((name, Some(root)));
    }
    basename_with_alt_separators(cwd).map(|name| (name, None))
}

/// Basename tolerant of both `/` and `\` separators. Windows agents
/// send a Windows path over JSON to a Linux server; the server still
/// has to land the row under a deterministic project name. `Path::file_name`
/// alone uses the host's separator and would return `Some("c:\\users\\a")`
/// for a Windows path on Linux.
fn basename_with_alt_separators(path: &Path) -> Option<String> {
    let s = path.to_str()?;
    let name = s.rsplit(['/', '\\']).find(|seg| !seg.is_empty())?;
    Some(name.to_string())
}

/// Read commits, format each as a one-paragraph entry. We include
/// only commits with a substantive body (more than ~120 chars
/// total) — drive-by typo-fix commits aren't worth tokens.
fn collect_git_commits(
    repo_path: &Path,
    since: Option<&str>,
) -> Result<Vec<BootstrapSource>, BootstrapError> {
    let repo = match git2::Repository::open(repo_path) {
        Ok(repo) => repo,
        Err(err) => return collect_git_commits_fallback(repo_path, since, err),
    };
    collect_git_commits_git2(&repo, since)
}

fn collect_git_commits_git2(
    repo: &git2::Repository,
    since: Option<&str>,
) -> Result<Vec<BootstrapSource>, BootstrapError> {
    let mut revwalk = repo.revwalk()?;
    revwalk.set_sorting(git2::Sort::TIME)?;
    revwalk.push_head()?;
    // `since` filtering — best-effort. libgit2's revwalk doesn't have
    // direct time-since, so we parse commit timestamps + compare to
    // a target. For simplicity we skip the since-filter when set but
    // unparsable, rather than failing loudly.
    let since_epoch = since.and_then(parse_since_to_epoch);

    let mut out = Vec::new();
    for oid in revwalk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        if let Some(epoch) = since_epoch
            && commit.time().seconds() < epoch
        {
            break; // revwalk is time-sorted; older commits come next
        }
        let summary = commit.summary().ok().flatten().unwrap_or("(no summary)");
        let body = commit.body().ok().flatten().unwrap_or("");
        let combined_len = summary.len() + body.len();
        // Skip trivial commits.
        if combined_len < 120 && !is_conventional_substantive(summary) {
            continue;
        }
        let author = commit.author().name().unwrap_or("unknown").to_string();
        let when = jiff::Timestamp::from_second(commit.time().seconds())
            .map(|t| t.to_string())
            .unwrap_or_else(|_| commit.time().seconds().to_string());
        let label = format!("git: {summary}");
        let text = format!(
            "Commit {short}\nAuthor: {author}\nDate: {when}\n\n{summary}\n\n{body}",
            short = oid.to_string().chars().take(8).collect::<String>(),
        );
        out.push(BootstrapSource {
            kind: SourceKind::GitCommit,
            label,
            text,
        });
    }
    debug!(count = out.len(), "collected git commits");
    Ok(out)
}

#[cfg(windows)]
fn collect_git_commits_fallback(
    repo_path: &Path,
    since: Option<&str>,
    original: git2::Error,
) -> Result<Vec<BootstrapSource>, BootstrapError> {
    warn!(
        error = %original,
        path = %repo_path.display(),
        "git2 failed to open repository; falling back to git CLI"
    );
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args([
            "log",
            "--date=unix",
            "--format=%H%x1f%an%x1f%ct%x1f%s%x1f%b%x1e",
        ])
        .output()?;
    if !output.status.success() {
        return Err(BootstrapError::Git(original));
    }

    let since_epoch = since.and_then(parse_since_to_epoch);
    let mut out = Vec::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for record in stdout.split('\x1e') {
        let record = record.trim_matches(['\r', '\n']);
        if record.is_empty() {
            continue;
        }
        let mut fields = record.splitn(5, '\x1f');
        let Some(hash) = fields.next() else { continue };
        let author = fields.next().filter(|s| !s.is_empty()).unwrap_or("unknown");
        let epoch = fields
            .next()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        let summary = fields
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or("(no summary)");
        let body = fields.next().unwrap_or("").trim_start_matches(['\r', '\n']);

        if let Some(cutoff) = since_epoch
            && epoch < cutoff
        {
            break;
        }
        let combined_len = summary.len() + body.len();
        if combined_len < 120 && !is_conventional_substantive(summary) {
            continue;
        }
        let when = jiff::Timestamp::from_second(epoch)
            .map(|t| t.to_string())
            .unwrap_or_else(|_| epoch.to_string());
        let short = hash.chars().take(8).collect::<String>();
        out.push(BootstrapSource {
            kind: SourceKind::GitCommit,
            label: format!("git: {summary}"),
            text: format!("Commit {short}\nAuthor: {author}\nDate: {when}\n\n{summary}\n\n{body}"),
        });
    }
    debug!(count = out.len(), "collected git commits");
    Ok(out)
}

#[cfg(not(windows))]
fn collect_git_commits_fallback(
    _repo_path: &Path,
    _since: Option<&str>,
    original: git2::Error,
) -> Result<Vec<BootstrapSource>, BootstrapError> {
    Err(BootstrapError::Git(original))
}

/// Parse a `git log --since=<x>` value into a unix epoch. Supports
/// the simplest formats we expect operators to type:
/// "30 days ago", "180 days ago", "1 year ago", or an absolute YYYY-MM-DD.
fn parse_since_to_epoch(since: &str) -> Option<i64> {
    let lower = since.to_lowercase();
    let now = Timestamp::now().as_second();
    if let Some(rest) = lower.strip_suffix(" days ago") {
        let n: i64 = rest.trim().parse().ok()?;
        return Some(now - n * 86_400);
    }
    if let Some(rest) = lower.strip_suffix(" months ago") {
        let n: i64 = rest.trim().parse().ok()?;
        return Some(now - n * 30 * 86_400);
    }
    if let Some(rest) = lower.strip_suffix(" year ago") {
        let n: i64 = rest.trim().parse().ok()?;
        return Some(now - n * 365 * 86_400);
    }
    if let Some(rest) = lower.strip_suffix(" years ago") {
        let n: i64 = rest.trim().parse().ok()?;
        return Some(now - n * 365 * 86_400);
    }
    // YYYY-MM-DD fallback.
    if let Ok(date) = jiff::civil::Date::strptime("%Y-%m-%d", &lower)
        && let Ok(zoned) = date.to_zoned(jiff::tz::TimeZone::UTC)
    {
        return Some(zoned.timestamp().as_second());
    }
    None
}

/// Conventional-commit prefixes worth keeping even when the body
/// is short — they're explicitly typed as significant by the author.
fn is_conventional_substantive(summary: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "feat:",
        "feat(",
        "fix:",
        "fix(",
        "refactor:",
        "refactor(",
        "perf:",
        "perf(",
        "design:",
        "design(",
    ];
    PREFIXES.iter().any(|p| summary.starts_with(p))
}

fn collect_readme(repo_path: &Path) -> Result<Vec<BootstrapSource>, BootstrapError> {
    for candidate in ["README.md", "README", "Readme.md", "readme.md"] {
        let p = repo_path.join(candidate);
        if p.is_file() {
            let text = std::fs::read_to_string(&p)?;
            return Ok(vec![BootstrapSource {
                kind: SourceKind::Readme,
                label: format!("README ({})", candidate),
                text,
            }]);
        }
    }
    Ok(Vec::new())
}

fn collect_docs(repo_path: &Path) -> Result<Vec<BootstrapSource>, BootstrapError> {
    let docs_dir = repo_path.join("docs");
    if !docs_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut stack = vec![docs_dir];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("md")
                && let Ok(text) = std::fs::read_to_string(&path)
            {
                let label = path
                    .strip_prefix(repo_path)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                out.push(BootstrapSource {
                    kind: SourceKind::DocFile,
                    label: format!("doc: {label}"),
                    text,
                });
            }
        }
    }
    debug!(count = out.len(), "collected docs/*.md");
    Ok(out)
}

/// Pull module-level `//!` doc-comments from the top of every .rs
/// file (skip the build/target/vendor/.git tree). Stops at the first
/// non-`//!` line so test-only files with implementation noise
/// don't dump source into the prompt.
fn collect_rust_module_headers(repo_path: &Path) -> Result<Vec<BootstrapSource>, BootstrapError> {
    let mut out = Vec::new();
    let mut stack = vec![repo_path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if matches!(name, "target" | "node_modules" | ".git" | "vendor") {
            continue;
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for entry in read.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                stack.push(path);
            } else if path.extension().and_then(|s| s.to_str()) == Some("rs")
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                let mut header_lines = Vec::new();
                for line in content.lines() {
                    let trimmed = line.trim_start();
                    if trimmed.starts_with("//!") {
                        header_lines.push(trimmed.trim_start_matches("//!").trim());
                    } else if header_lines.is_empty() && trimmed.is_empty() {
                        continue;
                    } else {
                        break;
                    }
                }
                if header_lines.is_empty() {
                    continue;
                }
                let label = path
                    .strip_prefix(repo_path)
                    .unwrap_or(&path)
                    .display()
                    .to_string();
                let text = header_lines.join("\n");
                out.push(BootstrapSource {
                    kind: SourceKind::ModuleHeader,
                    label: format!("rust: {label}"),
                    text,
                });
            }
        }
    }
    debug!(count = out.len(), "collected rust module headers");
    Ok(out)
}

fn collect_project_rules(repo_path: &Path) -> Result<Vec<BootstrapSource>, BootstrapError> {
    let mut out = Vec::new();
    for name in ["CLAUDE.md", "AGENTS.md", "AGENT.md", "claude.md"] {
        let p = repo_path.join(name);
        if p.is_file()
            && let Ok(text) = std::fs::read_to_string(&p)
        {
            out.push(BootstrapSource {
                kind: SourceKind::ProjectRules,
                label: format!("rules: {name}"),
                text,
            });
        }
    }
    Ok(out)
}

// --------------------------------------------------------------------
// Budgeting
// --------------------------------------------------------------------

/// Drop lower-priority sources until estimated tokens fit under the
/// budget. Returns (kept, dropped_count, estimated_total_tokens).
pub fn prune_sources_to_budget(
    mut sources: Vec<BootstrapSource>,
    budget: usize,
) -> (Vec<BootstrapSource>, usize, usize) {
    // Reserve ~1k tokens for the prompt scaffolding itself.
    let usable = budget.saturating_sub(1_000);
    // Order: highest drop_priority FIRST → drop those when over budget.
    sources.sort_by_key(|s| std::cmp::Reverse(s.kind.drop_priority()));
    let total_count = sources.len();
    let mut total: usize = sources.iter().map(BootstrapSource::estimated_tokens).sum();

    // We iterate sources in drop order, then drain once. Repeated
    // `remove(0)` shifts the whole vector each time on large repos.
    let mut drop_count = 0;
    while total > usable
        && let Some(victim) = sources.get(drop_count)
    {
        total = total.saturating_sub(victim.estimated_tokens());
        drop_count += 1;
    }
    sources.drain(0..drop_count);
    let kept = sources;
    let dropped = total_count - kept.len();
    (kept, dropped, total)
}

/// Default per-chunk input budget when chunking is enabled (~24k tokens
/// leaves headroom for system prompt + JSON output on Cursor-class providers).
pub const DEFAULT_CHUNK_INPUT_TOKENS: usize = 24_000;

/// Clamp per-chunk budget to the total input cap. `chunk_input_tokens == 0`
/// disables chunking (single LLM call).
#[must_use]
pub fn effective_chunk_budget(chunk_input_tokens: usize, max_input_tokens: usize) -> usize {
    if chunk_input_tokens == 0 {
        0
    } else {
        chunk_input_tokens.min(max_input_tokens)
    }
}

/// Outcome counters: honour a client-side pre-prune count when supplied.
fn bootstrap_source_counts(
    incoming: usize,
    sources_collected: Option<usize>,
    kept: &[BootstrapSource],
) -> (usize, usize, usize) {
    let collected = sources_collected.unwrap_or(incoming);
    let sources_sent = kept.len();
    let sources_dropped = collected.saturating_sub(sources_sent);
    (collected, sources_sent, sources_dropped)
}

/// Split pruned sources into LLM-sized chunks. `chunk_budget` of `0` returns
/// a single chunk containing all sources (legacy one-shot behaviour).
#[must_use]
pub fn plan_bootstrap_chunks(
    sources: Vec<BootstrapSource>,
    chunk_budget: usize,
) -> Vec<Vec<BootstrapSource>> {
    if sources.is_empty() {
        return Vec::new();
    }
    if chunk_budget == 0 {
        return vec![sources];
    }
    let usable = usable_chunk_tokens(chunk_budget);
    let total: usize = sources.iter().map(BootstrapSource::estimated_tokens).sum();
    if total <= usable {
        return vec![sources];
    }
    chunk_sources_greedy(sources, usable)
}

/// Pack sources into chunks ≤ `usable` estimated tokens (best sources first).
fn chunk_sources_greedy(sources: Vec<BootstrapSource>, usable: usize) -> Vec<Vec<BootstrapSource>> {
    let mut chunks: Vec<Vec<BootstrapSource>> = Vec::new();
    let mut current: Vec<BootstrapSource> = Vec::new();
    let mut current_tokens = 0usize;

    // After prune, low drop_priority (README, rules) sit at the end — process
    // those first so early chunks carry the highest-signal material.
    for src in sources
        .into_iter()
        .rev()
        .flat_map(|src| split_oversized_source(src, usable))
    {
        let t = src.estimated_tokens();
        if !current.is_empty() && current_tokens.saturating_add(t) > usable {
            chunks.push(current);
            current = Vec::new();
            current_tokens = 0;
        }
        current_tokens = current_tokens.saturating_add(t);
        current.push(src);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn usable_chunk_tokens(chunk_budget: usize) -> usize {
    const PROMPT_RESERVE: usize = 1_000;
    if chunk_budget > PROMPT_RESERVE {
        chunk_budget - PROMPT_RESERVE
    } else {
        chunk_budget
    }
}

fn split_oversized_source(source: BootstrapSource, usable: usize) -> Vec<BootstrapSource> {
    if usable == 0 || source.estimated_tokens() <= usable {
        return vec![source];
    }

    let BootstrapSource { kind, label, text } = source;
    let max_text_chars = usable
        .saturating_mul(CHARS_PER_TOKEN)
        .saturating_sub(label.len().saturating_add(64))
        .max(CHARS_PER_TOKEN);
    let mut parts = Vec::new();
    let mut start = 0usize;
    while start < text.len() {
        let mut end = start.saturating_add(max_text_chars).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = text[start..]
                .char_indices()
                .nth(1)
                .map_or(text.len(), |(idx, _)| start + idx);
        }
        let part_no = parts.len() + 1;
        parts.push(BootstrapSource {
            kind,
            label: format!("{label} (part {part_no})"),
            text: text[start..end].to_string(),
        });
        start = end;
    }
    parts
}

// --------------------------------------------------------------------
// LLM prompt
// --------------------------------------------------------------------

/// System prompt for bootstrap. Loaded at compile time from
/// `prompts/bootstrap_system.md`.
const SYSTEM_PROMPT: &str = include_str!("../prompts/bootstrap_system.md");

fn build_chunk_request(
    sources: &[BootstrapSource],
    chunk_index: usize,
    chunk_total: usize,
    prior_paths: &[&str],
) -> ChatRequest {
    let mut buf = String::with_capacity(8_192);
    if chunk_total > 1 {
        buf.push_str(&format!(
            "CHUNK {chunk_index} of {chunk_total}. Process ONLY the sources in this message.\n"
        ));
        if !prior_paths.is_empty() {
            buf.push_str(
                "Wiki pages already produced in earlier chunks (do not duplicate unless \
                 you are merging new facts into the same path):\n",
            );
            for p in prior_paths {
                buf.push_str(&format!("- `{p}`\n"));
            }
            buf.push('\n');
            let next_adr = next_decision_serial(prior_paths);
            buf.push_str(&format!(
                "For new `decisions/` pages, continue numbering at {next_adr:04} \
                 (e.g. `decisions/{next_adr:04}-<slug>.md`).\n\n"
            ));
        }
        buf.push_str(
            "Prefer 3-8 substantive pages for this chunk. Skip topics already covered above.\n\n",
        );
    } else {
        buf.push_str("Sources collected from the project. Convert into wiki pages.\n\n");
    }
    for src in sources {
        buf.push_str(&format!("=== {} ===\n", src.label));
        buf.push_str(&src.text);
        buf.push_str("\n\n");
    }
    let max_tokens = if chunk_total > 1 {
        // Each chunk targets a smaller page batch; keeps Cursor bridge responses bounded.
        16_000
    } else {
        64_000
    };
    ChatRequest {
        system: Some(SYSTEM_PROMPT.into()),
        messages: vec![ChatMessage {
            role: Role::User,
            content: buf,
        }],
        max_tokens,
        temperature: Some(0.2),
    }
}

/// Highest `decisions/NNNN-…` serial seen in `prior_paths`, or 0.
fn next_decision_serial(prior_paths: &[&str]) -> u32 {
    prior_paths
        .iter()
        .filter_map(|p| parse_decision_serial(p))
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

fn parse_decision_serial(path: &str) -> Option<u32> {
    let rest = path.strip_prefix("decisions/")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

fn insert_bootstrap_page(
    pages_by_path: &mut std::collections::BTreeMap<String, BootstrapPage>,
    page: BootstrapPage,
    chunk_index: usize,
) {
    if pages_by_path.contains_key(&page.path) {
        warn!(
            path = %page.path,
            chunk = chunk_index,
            "skipping duplicate bootstrap page path from later chunk"
        );
        return;
    }
    pages_by_path.insert(page.path.clone(), page);
}

// --------------------------------------------------------------------
// Manifest rendering
// --------------------------------------------------------------------

fn build_frontmatter(title: &str, tags: &[String], now: Timestamp) -> serde_json::Value {
    serde_json::json!({
        "title": title,
        "tier": "semantic",
        "tags": tags,
        "bootstrapped_at": now.to_string(),
    })
}

struct ManifestRender<'a> {
    now: Timestamp,
    sources_collected: usize,
    sources_sent: usize,
    sources_dropped: usize,
    est_tokens: usize,
    rationale: &'a str,
    pages: &'a [String],
    llm_chunks: usize,
}

fn render_manifest_body(input: ManifestRender<'_>) -> String {
    let ManifestRender {
        now,
        sources_collected,
        sources_sent,
        sources_dropped,
        est_tokens,
        rationale,
        pages,
        llm_chunks,
    } = input;
    let mut buf = String::with_capacity(1024);
    buf.push_str("# Bootstrap manifest\n\n");
    buf.push_str(&format!(
        "> Generated by `ai-memory bootstrap` at {now}.\n\n",
    ));
    buf.push_str("## Sources\n\n");
    buf.push_str(&format!(
        "- Collected: **{sources_collected}**\n\
         - Sent to LLM: **{sources_sent}**\n\
         - Dropped to fit budget: **{sources_dropped}**\n\
         - Estimated input tokens: **{est_tokens}**\n\
         - LLM chunks: **{llm_chunks}**\n\n"
    ));
    buf.push_str("## Rationale\n\n");
    buf.push_str(rationale);
    buf.push_str("\n\n## Pages produced\n\n");
    for p in pages {
        buf.push_str(&format!("- `{p}`\n"));
    }
    buf.push_str("\n---\n\n");
    buf.push_str(
        "_Re-running `ai-memory bootstrap` requires `--force` while this page \
         exists. Delete this page (and the generated ones below) to reset._\n",
    );
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn make_repo(tmp: &Path) -> Result<(), Box<dyn std::error::Error>> {
        // Use system git via std::process::Command instead of git2's
        // signature/config plumbing, which would force us to set a
        // committer identity in the test.
        let run = |args: &[&str]| -> std::io::Result<()> {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(tmp)
                .status()?;
            assert!(status.success(), "git {args:?} failed");
            Ok(())
        };
        run(&["init", "-q", "-b", "main"])?;
        run(&["config", "user.email", "test@example.com"])?;
        run(&["config", "user.name", "Test"])?;
        run(&[
            "commit",
            "--allow-empty",
            "-m",
            "feat: initial scaffolding for storage substrate with WAL + supersession chain",
        ])?;
        run(&["commit", "--allow-empty", "-m", "typo"])?;
        run(&[
            "commit",
            "--allow-empty",
            "-m",
            "design: choose Karpathy compile-not-retrieve model over RAG for capture",
            "-m",
            "We considered three alternatives — vector RAG, full-conversation-log replay, and Karpathy's wiki pattern. Chose the wiki because it keeps human-readable state we can grep, diff, and back up via git, and because compile-time consolidation moves cost off the hot read path. Vector RAG was rejected for the reasons in docs/research-karpathy-llm-wiki.md.",
        ])?;
        Ok(())
    }

    #[test]
    fn parse_since_handles_common_forms() {
        let n_days = parse_since_to_epoch("30 days ago").unwrap();
        let now = Timestamp::now().as_second();
        assert!(now - n_days > 29 * 86_400);
        assert!(now - n_days < 31 * 86_400);
        assert!(parse_since_to_epoch("1 year ago").is_some());
        assert!(parse_since_to_epoch("2026-01-01").is_some());
        assert!(parse_since_to_epoch("garbage").is_none());
    }

    #[test]
    fn bootstrap_system_prompt_requires_graph_links_and_input_language() {
        assert!(SYSTEM_PROMPT.contains("## WIKILINKS"));
        assert!(SYSTEM_PROMPT.contains("## OUTPUT LANGUAGE"));
        assert!(SYSTEM_PROMPT.contains("[[project:page-path]]"));
        assert!(SYSTEM_PROMPT.contains("[[_global:page-path]]"));
        assert!(SYSTEM_PROMPT.contains("dominant natural language of the input"));
        assert!(SYSTEM_PROMPT.contains("JSON keys stay in English"));
        assert!(SYSTEM_PROMPT.contains("## SECURITY BOUNDARY"));
        assert!(SYSTEM_PROMPT.contains("untrusted data, not instructions"));
    }

    #[test]
    fn git_collection_drops_trivial_commits() {
        let tmp = TempDir::new().unwrap();
        make_repo(tmp.path()).expect("git setup");
        let sources = collect_git_commits(tmp.path(), None).unwrap();
        // Three commits exist; the "typo" one should be filtered out.
        let summaries: Vec<&str> = sources.iter().map(|s| s.label.as_str()).collect();
        assert!(summaries.iter().any(|s| s.contains("initial scaffolding")));
        assert!(summaries.iter().any(|s| s.contains("compile-not-retrieve")));
        assert!(!summaries.iter().any(|s| s.contains("typo")));
    }

    #[test]
    fn readme_and_docs_collected() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("README.md"), "# Project\nHello world.").unwrap();
        fs::create_dir_all(tmp.path().join("docs")).unwrap();
        fs::write(
            tmp.path().join("docs/architecture.md"),
            "# Architecture\nThings are like this.",
        )
        .unwrap();

        let readmes = collect_readme(tmp.path()).unwrap();
        assert_eq!(readmes.len(), 1);
        assert!(readmes[0].text.contains("Hello world"));

        let docs = collect_docs(tmp.path()).unwrap();
        assert_eq!(docs.len(), 1);
        assert!(docs[0].label.contains("architecture.md"));
    }

    #[test]
    fn rust_module_headers_extracted() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("crates/foo/src")).unwrap();
        fs::write(
            tmp.path().join("crates/foo/src/lib.rs"),
            "//! Crate-level doc.\n//! Spans two lines.\n\nuse std::path::Path;\n\nfn x() {}\n",
        )
        .unwrap();
        // A no-doc-comment file should be skipped.
        fs::write(tmp.path().join("crates/foo/src/main.rs"), "fn main() {}\n").unwrap();
        let sources = collect_rust_module_headers(tmp.path()).unwrap();
        assert_eq!(sources.len(), 1, "only the doc-commented file");
        assert!(sources[0].text.contains("Crate-level doc"));
        assert!(sources[0].text.contains("two lines"));
        assert!(!sources[0].text.contains("std::path"));
    }

    #[test]
    fn rust_module_header_skips_target_dir() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
        fs::write(
            tmp.path().join("target/debug/foo.rs"),
            "//! Build artefact; must not be ingested.\n",
        )
        .unwrap();
        let sources = collect_rust_module_headers(tmp.path()).unwrap();
        assert!(sources.is_empty(), "target/ must be excluded");
    }

    #[test]
    fn prune_to_budget_drops_low_priority_first() {
        let big_header = BootstrapSource {
            kind: SourceKind::ModuleHeader,
            label: "rust: x.rs".into(),
            text: "x".repeat(40_000),
        };
        let readme = BootstrapSource {
            kind: SourceKind::Readme,
            label: "README".into(),
            text: "important".to_string(),
        };
        let (kept, dropped, _) = prune_sources_to_budget(vec![big_header, readme], 1_500);
        assert_eq!(dropped, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].kind, SourceKind::Readme);
    }

    #[test]
    fn prune_keeps_everything_when_under_budget() {
        let s1 = BootstrapSource {
            kind: SourceKind::GitCommit,
            label: "g".into(),
            text: "short".into(),
        };
        let s2 = BootstrapSource {
            kind: SourceKind::Readme,
            label: "r".into(),
            text: "shorter".into(),
        };
        let (kept, dropped, _) = prune_sources_to_budget(vec![s1, s2], 50_000);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn plan_chunks_single_when_under_budget() {
        let s = BootstrapSource {
            kind: SourceKind::Readme,
            label: "README".into(),
            text: "hello".into(),
        };
        let chunks = plan_bootstrap_chunks(vec![s], DEFAULT_CHUNK_INPUT_TOKENS);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn plan_chunks_splits_when_over_budget() {
        let mut sources = Vec::new();
        for i in 0..8 {
            sources.push(BootstrapSource {
                kind: SourceKind::GitCommit,
                label: format!("git: commit {i}"),
                text: "x".repeat(12_000),
            });
        }
        let chunks = plan_bootstrap_chunks(sources, 8_000);
        assert!(chunks.len() > 1, "expected multiple chunks");
        for chunk in &chunks {
            let t: usize = chunk.iter().map(BootstrapSource::estimated_tokens).sum();
            assert!(t <= 8_000, "chunk should respect usable budget");
        }
    }

    #[test]
    fn plan_chunks_splits_single_oversized_source() {
        let source = BootstrapSource {
            kind: SourceKind::DocFile,
            label: "docs/huge.md".into(),
            text: "x".repeat(100_000),
        };
        let chunks = plan_bootstrap_chunks(vec![source], 8_000);
        assert!(chunks.len() > 1, "one large file should be split");
        for chunk in &chunks {
            let t: usize = chunk.iter().map(BootstrapSource::estimated_tokens).sum();
            assert!(t <= 8_000, "chunk should respect budget; got {t}");
        }
        assert!(
            chunks
                .iter()
                .flatten()
                .any(|source| source.label.contains("(part 2)")),
            "split labels should identify later parts"
        );
    }

    #[test]
    fn plan_chunks_treats_tiny_chunk_budget_as_content_budget() {
        let source = BootstrapSource {
            kind: SourceKind::DocFile,
            label: "docs/huge.md".into(),
            text: "x".repeat(10_000),
        };
        let chunks = plan_bootstrap_chunks(vec![source], 900);
        assert!(chunks.len() > 1, "tiny non-zero budget should still split");
        for chunk in &chunks {
            let t: usize = chunk.iter().map(BootstrapSource::estimated_tokens).sum();
            assert!(t <= 900, "chunk should respect tiny budget; got {t}");
        }
    }

    #[test]
    fn plan_chunks_zero_disables() {
        let s = BootstrapSource {
            kind: SourceKind::Readme,
            label: "r".into(),
            text: "x".repeat(100_000),
        };
        let chunks = plan_bootstrap_chunks(vec![s], 0);
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn source_counts_honour_client_collected_hint() {
        let kept = vec![BootstrapSource {
            kind: SourceKind::Readme,
            label: "README".into(),
            text: "x".into(),
        }];
        let (collected, sent, dropped) = bootstrap_source_counts(1, Some(27_873), &kept);
        assert_eq!(collected, 27_873);
        assert_eq!(sent, 1);
        assert_eq!(dropped, 27_872);
    }

    #[test]
    fn batch_without_pages_key_deserialises_as_empty() {
        let batch: BootstrapBatch =
            serde_json::from_str(r#"{"rationale":"already covered by earlier chunks"}"#)
                .expect("a chunk that adds no pages must not fail the whole run");

        assert!(batch.pages.is_empty());
        assert_eq!(batch.rationale, "already covered by earlier chunks");
    }

    #[test]
    fn duplicate_page_paths_keep_first_page() {
        let mut pages = std::collections::BTreeMap::new();
        insert_bootstrap_page(
            &mut pages,
            BootstrapPage {
                path: "concepts/runtime.md".into(),
                title: "Runtime".into(),
                body_markdown: "first".into(),
                tags: vec![],
            },
            1,
        );
        insert_bootstrap_page(
            &mut pages,
            BootstrapPage {
                path: "concepts/runtime.md".into(),
                title: "Runtime duplicate".into(),
                body_markdown: "second".into(),
                tags: vec![],
            },
            2,
        );

        assert_eq!(pages.len(), 1);
        assert_eq!(pages["concepts/runtime.md"].body_markdown, "first");
    }

    #[test]
    fn effective_chunk_budget_clamps_to_max() {
        assert_eq!(effective_chunk_budget(50_000, 24_000), 24_000);
        assert_eq!(effective_chunk_budget(0, 24_000), 0);
    }

    #[test]
    fn next_decision_serial_continues_across_chunks() {
        let paths = [
            "concepts/foo.md",
            "decisions/0003-old.md",
            "decisions/0001-first.md",
        ];
        assert_eq!(next_decision_serial(&paths), 4);
        assert_eq!(next_decision_serial(&[]), 1);
    }

    /// Basename strategy returns just `basename(cwd)` with no repo
    /// walk. Used by hook agents that opt out of the worktree
    /// collapse (`AI_MEMORY_PROJECT_STRATEGY=basename`).
    #[test]
    fn derive_project_name_basename_strategy() {
        let p = Path::new("/home/alice/projects/foo-app");
        let (name, root) = derive_project_name(p, ProjectNameStrategy::Basename).unwrap();
        assert_eq!(name, "foo-app");
        assert!(root.is_none(), "basename never returns a repo path");
    }

    /// Windows-style path with `\` separators submitted by a Windows
    /// agent must still resolve to the final component on a Linux
    /// server. `Path::file_name` alone would treat the whole string
    /// as a single component.
    #[test]
    fn derive_project_name_handles_backslash_separators() {
        let p = Path::new(r"C:\Users\alice\Projects\my-app");
        let (name, _) = derive_project_name(p, ProjectNameStrategy::Basename).unwrap();
        assert_eq!(name, "my-app");
    }

    #[test]
    fn main_root_from_common_dir_rejects_bare_repositories() {
        let start = Path::new("/tmp/repo.git");
        assert!(main_root_from_common_dir(Path::new("/tmp/repo.git"), true, start).is_err());
    }

    #[test]
    fn main_root_from_common_dir_returns_parent_for_worktree_common_dir() {
        let root =
            main_root_from_common_dir(Path::new("/tmp/repo/.git"), false, Path::new("/tmp/repo"))
                .unwrap();
        assert_eq!(root, PathBuf::from("/tmp/repo"));
    }

    /// Degenerate input (root, empty) returns None instead of
    /// returning an empty project name that would later fail
    /// `get_or_create_project`'s validation.
    #[test]
    fn derive_project_name_rejects_degenerate_paths() {
        assert!(derive_project_name(Path::new("/"), ProjectNameStrategy::Basename).is_none());
        assert!(derive_project_name(Path::new(""), ProjectNameStrategy::Basename).is_none());
    }

    /// `MainRepoRoot` strategy walks up to the main repo and uses its
    /// basename. The synthetic repo here lives at `<tmp>/repo`, so
    /// resolving from `<tmp>/repo/sub/dir` returns `"repo"` plus the
    /// repo path.
    #[test]
    fn derive_project_name_main_repo_root_collapses_subdirs() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("sub").join("dir")).unwrap();
        // Reuse the in-test git scaffolding so the lookup finds a repo.
        let run = |args: &[&str]| -> std::io::Result<()> {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .status()?;
            assert!(status.success(), "git {args:?} failed");
            Ok(())
        };
        run(&["init", "-q", "-b", "main"]).unwrap();
        run(&["config", "user.email", "t@t"]).unwrap();
        run(&["config", "user.name", "t"]).unwrap();
        run(&["commit", "--allow-empty", "-m", "init"]).unwrap();

        let from_sub = repo.join("sub").join("dir");
        let (name, root) =
            derive_project_name(&from_sub, ProjectNameStrategy::MainRepoRoot).unwrap();
        assert_eq!(name, "repo");
        // git2::Repository::discover follows real-path symlinks; compare
        // by canonicalised form so a /private/var vs /var prefix on
        // macOS or similar doesn't trip the assertion.
        assert_eq!(
            root.as_ref().and_then(|p| p.canonicalize().ok()),
            Some(repo.canonicalize().unwrap()),
        );
    }

    /// `MainRepoRoot` falls back to `Basename` semantics when no git
    /// repo lives at or above the path — never errors, never returns
    /// `("", None)`.
    #[test]
    fn derive_project_name_main_repo_root_falls_back_to_basename() {
        let tmp = TempDir::new().unwrap();
        let plain = tmp.path().join("plain-dir");
        std::fs::create_dir_all(&plain).unwrap();
        let (name, root) = derive_project_name(&plain, ProjectNameStrategy::MainRepoRoot).unwrap();
        assert_eq!(name, "plain-dir");
        assert!(
            root.is_none(),
            "no git repo found ⇒ no repo path in the tuple"
        );
    }
}
