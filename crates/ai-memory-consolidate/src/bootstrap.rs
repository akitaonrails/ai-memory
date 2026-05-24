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
//! Input is capped via `max_input_tokens` (default 50k). We
//! estimate token count locally at chars/4 (the standard rough
//! heuristic) and drop lower-priority sources first when over
//! budget. At Kimi's $0.73/$3.49 per M, 50k input ≈ $0.04;
//! generated output (1-2k tokens) ≈ $0.007. Worst case under
//! $0.20 per run.
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

        let collected = sources.len();
        let (kept, dropped, est_tokens) = prune_to_budget(sources, cfg.max_input_tokens);
        info!(
            collected,
            kept = kept.len(),
            dropped,
            est_tokens,
            "bootstrap sources prioritised + budget-capped",
        );

        let kept_counts = SourceCounts::from_sources(&kept);

        // ---- dry run early-exits before the LLM call --------------
        if cfg.dry_run {
            return Ok(BootstrapOutcome {
                sources_collected: collected,
                sources_sent: kept.len(),
                sources_dropped: dropped,
                sources_by_kind: kept_counts,
                estimated_input_tokens: est_tokens,
                pages_written: Vec::new(),
                rationale: "(dry-run; LLM not invoked)".to_string(),
                dry_run: true,
            });
        }

        // ---- LLM call ---------------------------------------------
        let request = build_request(&kept);
        let batch: BootstrapBatch = complete_structured(&*self.llm, request).await?;

        // ---- write pages ------------------------------------------
        let now = Timestamp::now();
        let mut requests = Vec::with_capacity(batch.pages.len() + 1);
        let mut written_paths = Vec::with_capacity(batch.pages.len() + 1);
        for page in &batch.pages {
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
            });
        }
        // Plus the manifest itself.
        let manifest_body = render_manifest_body(
            now,
            collected,
            kept.len(),
            dropped,
            est_tokens,
            &batch.rationale,
            &written_paths,
        );
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
        });

        let _ids = self.wiki.apply_batch(requests).await?;
        let _ = self.wiki.commit_all(&format!(
            "bootstrap: ingested {collected} sources, wrote {} pages",
            written_paths.len() + 1,
        ));

        // Manifest is the last entry but conceptually first; list it.
        let mut out_paths = written_paths.clone();
        out_paths.push("bootstrap.md".to_string());

        Ok(BootstrapOutcome {
            sources_collected: collected,
            sources_sent: kept.len(),
            sources_dropped: dropped,
            sources_by_kind: kept_counts,
            estimated_input_tokens: est_tokens,
            pages_written: out_paths,
            rationale: batch.rationale,
            dry_run: false,
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

/// Read commits, format each as a one-paragraph entry. We include
/// only commits with a substantive body (more than ~120 chars
/// total) — drive-by typo-fix commits aren't worth tokens.
fn collect_git_commits(
    repo_path: &Path,
    since: Option<&str>,
) -> Result<Vec<BootstrapSource>, BootstrapError> {
    let repo = git2::Repository::open(repo_path)
        .map_err(|_| BootstrapError::NotARepo(repo_path.to_path_buf()))?;
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
        let summary = commit.summary().unwrap_or("(no summary)");
        let body = commit.body().unwrap_or("");
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
/// Drop lowest-priority sources until the total estimated tokens fit
/// under `budget`. Returns `(kept, dropped_count, kept_estimated_tokens)`.
///
/// Exposed so CLI dry-runs can reproduce the server's budget pass
/// without making the HTTP round-trip (which the bug fix for #N
/// short-circuits when `--dry-run` is set).
pub fn prune_to_budget(
    mut sources: Vec<BootstrapSource>,
    budget: usize,
) -> (Vec<BootstrapSource>, usize, usize) {
    // Reserve ~1k tokens for the prompt scaffolding itself.
    let usable = budget.saturating_sub(1_000);
    // Order: highest drop_priority FIRST → drop those when over budget.
    sources.sort_by_key(|s| std::cmp::Reverse(s.kind.drop_priority()));
    let total_count = sources.len();
    let mut total: usize = sources.iter().map(BootstrapSource::estimated_tokens).sum();

    // We iterate sources in drop order and remove until we fit.
    while total > usable
        && let Some(victim) = sources.first()
    {
        total = total.saturating_sub(victim.estimated_tokens());
        sources.remove(0);
    }
    let kept = sources;
    let dropped = total_count - kept.len();
    (kept, dropped, total)
}

// --------------------------------------------------------------------
// LLM prompt
// --------------------------------------------------------------------

/// System prompt for bootstrap. Loaded at compile time from
/// `prompts/bootstrap_system.md`.
const SYSTEM_PROMPT: &str = include_str!("../prompts/bootstrap_system.md");

fn build_request(sources: &[BootstrapSource]) -> ChatRequest {
    let mut buf = String::with_capacity(8_192);
    buf.push_str("Sources collected from the project. Convert into wiki pages.\n\n");
    for src in sources {
        buf.push_str(&format!("=== {} ===\n", src.label));
        buf.push_str(&src.text);
        buf.push_str("\n\n");
    }
    ChatRequest {
        system: Some(SYSTEM_PROMPT.into()),
        messages: vec![ChatMessage {
            role: Role::User,
            content: buf,
        }],
        // Generous budget so the model never runs out mid-response.
        // Truncated JSON is unrecoverable (no balanced `}`) — better
        // to over-allocate than to retry. 64K is Haiku/Sonnet 4.5's
        // max output; smaller hosted models clamp this server-side
        // to whatever they support, so the constant is safe to leave
        // generous for the headline providers.
        max_tokens: 64_000,
        temperature: Some(0.2),
    }
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

fn render_manifest_body(
    now: Timestamp,
    sources_collected: usize,
    sources_sent: usize,
    sources_dropped: usize,
    est_tokens: usize,
    rationale: &str,
    pages: &[String],
) -> String {
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
         - Estimated input tokens: **{est_tokens}**\n\n"
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
        let (kept, dropped, _) = prune_to_budget(vec![big_header, readme], 1_500);
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
        let (kept, dropped, _) = prune_to_budget(vec![s1, s2], 50_000);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
    }
}
