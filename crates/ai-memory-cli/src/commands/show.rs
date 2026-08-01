//! `ai-memory show` — pick a project and a harness, then launch it.
//!
//! Thin HTTP client for the listing half: calls `GET /api/v1/projects` and
//! renders the rows the server already aggregates. The launch half delegates
//! to [`crate::commands::run`] so managed workstreams, handoff delivery, and
//! native-argument forwarding keep exactly one implementation.
//!
//! The picker exists because project scope is resolved from the working
//! directory. Opening an agent from a parent directory that holds many
//! checkouts resolves every one of them to that parent's basename, so the
//! sessions pile into a single scope. Choosing the project first and entering
//! its recorded `repo_path` makes the scope explicit before the agent starts.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Stylize;
use crossterm::{cursor, execute, terminal};
use serde::Deserialize;

use crate::cli::{InstallInstructionsArgs, RunArgs, RunHarnessChoice, ShowArgs};
use crate::commands::apply_shared::apply_atomic;
use crate::commands::install_instructions;
use crate::config::Config;
use crate::http_client::{ServerEndpoint, get_json};

/// Server-shaped row. Mirrors `ai_memory_store::ProjectSummary`.
#[derive(Debug, Deserialize)]
struct ProjectRow {
    /// Workspace the project belongs to.
    workspace_name: String,
    /// Project name within the workspace.
    project_name: String,
    /// Number of `is_latest = 1` pages.
    page_count: u64,
    /// ISO-8601 timestamp of the newest page update, when any page exists.
    last_updated: Option<String>,
    /// Absolute checkout path, when one was recorded.
    repo_path: Option<String>,
}

/// One selectable line: what the user reads, plus the dimmed trailer.
struct Choice {
    /// Primary label, rendered at full brightness.
    label: String,
    /// Secondary detail, rendered dim. Empty renders nothing.
    detail: String,
}

/// Directory entries that mark a subdirectory as a project worth offering.
/// Matched by name, so recognising a candidate costs one `exists()` per marker
/// and never a directory walk.
const PROJECT_MARKERS: [&str; 12] = [
    ".git",
    ".ai-memory.toml",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "Gemfile",
    "composer.json",
    "pom.xml",
    "docker-compose.yml",
    "compose.yml",
];

/// Label of the always-present first entry that creates a project.
const NEW_PROJECT_LABEL: &str = "+ New project";

/// Workspace a created project is filed under when `--workspace` is absent.
/// Matches the scope resolver's own default, so a project created here lands
/// where a project created by the lifecycle hooks would.
const DEFAULT_WORKSPACE: &str = "default";

/// Directory names that hold dependencies or build output, never projects.
const SKIPPED_DIRS: [&str; 8] = [
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "venv",
    ".venv",
    "__pycache__",
];

/// One launchable entry: a project the server already tracks, or a directory
/// the scan just found.
struct Candidate {
    /// Workspace name when the server already tracks this checkout. `None`
    /// for a scanned directory, which lets `run` derive the scope from the
    /// checkout itself exactly as the lifecycle hooks would.
    workspace: Option<String>,
    /// Project name: the server's for tracked rows, the directory name for
    /// scanned ones.
    project: String,
    /// Directory to enter before launching.
    path: PathBuf,
    /// Dimmed trailer shown next to the label.
    detail: String,
}

/// Run the `show` subcommand.
///
/// # Errors
/// Returns an error if nothing launchable is found, stdin/stdout is not a
/// terminal, or the delegated launch fails. Returns the launched harness's
/// exit code.
pub async fn run(config: &Config, args: ShowArgs) -> Result<i32> {
    let mut candidates = tracked_projects(config, args.workspace.as_deref()).await;

    // The scan is what makes the picker useful on a machine where ai-memory
    // has not run yet: without it the menu stays empty until every project has
    // been opened once the long way, which is the very step this command
    // exists to remove.
    if !args.no_scan {
        let root = std::env::current_dir().context("reading the current directory")?;
        let mut seen: Vec<PathBuf> = candidates.iter().map(|c| normalize(&c.path)).collect();
        for path in scan_for_projects(&root) {
            let key = normalize(&path);
            if seen.contains(&key) {
                continue;
            }
            let Some(project) = directory_name(&path) else {
                continue;
            };
            seen.push(key);
            candidates.push(Candidate {
                workspace: None,
                project,
                path,
                detail: "not tracked yet".to_string(),
            });
        }
    }

    // Redirected output means nobody is there to press a key. Printing what
    // the menu would have offered is more useful than refusing: it makes the
    // list greppable, and `show` still answers "which projects can I launch?".
    // Creating a project needs a name nobody can type here, so that entry is
    // interactive-only and the printed list stays exactly the launchable set.
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        if candidates.is_empty() {
            bail!(
                "nothing to launch: the server tracks no project with a \
                 checkout path, and no project directory was found under {}. \
                 Run `ai-memory show` on a terminal to create one, or use \
                 `ai-memory run <harness>` directly",
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "the current directory".to_string())
            );
        }
        // Projects on stdout so the list stays greppable; the harness summary
        // on stderr so it never contaminates a pipeline.
        let detected: Vec<String> = RunHarnessChoice::value_variants()
            .iter()
            .filter(|choice| crate::commands::run::harness_available(**choice))
            .map(|choice| harness_name(*choice))
            .collect();
        eprintln!(
            "ai-memory: harnesses found in PATH: {}",
            if detected.is_empty() {
                "none".to_string()
            } else {
                detected.join(", ")
            }
        );
        for candidate in &candidates {
            println!(
                "{}\t{}\t{}",
                match &candidate.workspace {
                    Some(workspace) => format!("{workspace}/{}", candidate.project),
                    None => candidate.project.clone(),
                },
                candidate.path.display(),
                candidate.detail
            );
        }
        return Ok(0);
    }

    banner()?;

    // Creating a project leads the list because an empty machine has nothing
    // else to offer, and because the alternative — leave the menu, `mkdir`,
    // write a marker by hand, come back — is the same detour this command
    // exists to remove.
    let mut project_choices = vec![Choice {
        label: NEW_PROJECT_LABEL.to_string(),
        detail: "create the directory and its agent context files".to_string(),
    }];
    project_choices.extend(candidates.iter().map(|c| Choice {
        label: match &c.workspace {
            Some(workspace) => format!("{workspace}/{}", c.project),
            None => c.project.clone(),
        },
        detail: c.detail.clone(),
    }));
    let Some(picked) = select("Project", &project_choices)? else {
        return Ok(0);
    };

    // Ask for the name before the harness question: the answer decides which
    // directory the rest of the flow is about, and a name rejected after the
    // harness pick would throw that pick away.
    let root = std::env::current_dir().context("reading the current directory")?;
    let new_project = if picked == 0 {
        let Some(name) = prompt_project_name(&root)? else {
            return Ok(0);
        };
        Some(name)
    } else {
        None
    };

    // Offering a harness that is not installed only moves the failure one step
    // later, after the user has already committed to a project. Ask the same
    // question `run` would ask at launch, and ask it before the menu.
    let harnesses: Vec<RunHarnessChoice> = RunHarnessChoice::value_variants()
        .iter()
        .copied()
        .filter(|choice| crate::commands::run::harness_available(*choice))
        .collect();
    if harnesses.is_empty() {
        bail!(
            "no supported agent harness was found in PATH (looked for: {}). \
             Install one, or use `ai-memory run <harness> --executable <path>` \
             for a harness installed somewhere else",
            RunHarnessChoice::value_variants()
                .iter()
                .map(|h| harness_name(*h))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let harness_choices: Vec<Choice> = harnesses
        .iter()
        .map(|h| Choice {
            label: harness_name(*h),
            detail: String::new(),
        })
        .collect();
    let Some(picked_harness) = select(
        &format!(
            "Agent  ·  {}",
            new_project
                .as_deref()
                .unwrap_or(&project_choices[picked].label)
        ),
        &harness_choices,
    )?
    else {
        return Ok(0);
    };
    let harness = harnesses[picked_harness];

    // A created project pins nothing here: the marker written below already
    // declares both scope halves, so letting `run` read it keeps one source of
    // truth — the same one the lifecycle hooks will read on every later
    // session, whether or not it was launched from this menu.
    let (target, workspace, project) = match &new_project {
        Some(name) => {
            let workspace = args.workspace.as_deref().unwrap_or(DEFAULT_WORKSPACE);
            (create_project(&root, name, workspace)?, None, None)
        }
        None => {
            let chosen = &candidates[picked - 1];
            if !chosen.path.is_dir() {
                bail!(
                    "project {} records checkout {}, which no longer exists. \
                     Move it back, or launch from the new location once so the \
                     path is re-recorded",
                    chosen.project,
                    chosen.path.display()
                );
            }
            // A tracked row pins both scope halves, so a marker in the target
            // tree cannot retarget a launch the listing already resolved. A
            // scanned directory deliberately passes neither: `run` then derives
            // the scope from the checkout, honouring its `.ai-memory.toml`, and
            // the first session lands in the same scope the hooks would have
            // chosen on their own.
            (
                chosen.path.clone(),
                chosen.workspace.clone(),
                chosen.workspace.as_ref().map(|_| chosen.project.clone()),
            )
        }
    };

    // Entering the checkout is what makes every cwd-derived resolution inside
    // `run` (marker discovery, hook scope, transcript adoption) agree with the
    // project the user just picked.
    std::env::set_current_dir(&target).with_context(|| format!("entering {}", target.display()))?;

    // After the `cd`, because project-scoped skills resolve their root from the
    // working directory: installing from the parent would drop the skills next
    // to the sibling checkouts instead of inside the new project.
    if new_project.is_some() {
        install_context_files(config, harness, &target)?;
    }

    crate::commands::run::run(
        config,
        RunArgs {
            workspace,
            project,
            workstream: None,
            new_workstream: None,
            executable: None,
            yolo: args.yolo,
            fresh: args.fresh,
            harness: Some(harness),
            native_args: args.native_args,
        },
    )
    .await
}

/// Whether `name` is accepted by the server's workspace/project rule
/// (`^[a-z0-9][a-z0-9._-]*$`).
///
/// Checked here rather than at launch because a name the server rejects fails
/// inside the lifecycle hooks, as a warning on a session that already started —
/// long after the directory carrying that name exists on disk.
fn valid_scope_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
}

/// Marker contents pinning both scope halves for a freshly created project.
///
/// Both keys are written even though `project` defaults to the directory
/// basename: the default follows the directory, so a later rename or a
/// `cd` into a subdirectory would silently fork the memory into a second
/// project. Pinning them means the scope survives both.
fn marker_body(workspace: &str, project: &str) -> String {
    format!(
        "# Created by `ai-memory show`. Declares the scope every wiki page,\n\
         # observation, and handoff from this directory is filed under.\n\
         # Both keys are pinned on purpose: the defaults follow the directory\n\
         # name, so renaming or working from a subdirectory would otherwise\n\
         # start a second, empty project. See docs/marker-file.md.\n\
         workspace = \"{workspace}\"\n\
         project = \"{project}\"\n"
    )
}

/// Instruction file the routing block belongs in for `harness`.
///
/// Claude Code reads `CLAUDE.md`; every other supported harness converged on
/// `AGENTS.md`. Writing the one the chosen agent actually reads is what makes
/// the block take effect on the very first session.
const fn instruction_file(harness: RunHarnessChoice) -> &'static str {
    match harness {
        RunHarnessChoice::Claude => "CLAUDE.md",
        _ => "AGENTS.md",
    }
}

/// Create `name` under `parent` with its scope marker.
///
/// # Errors
/// Returns an error when the directory already exists or cannot be created,
/// or when the marker cannot be written.
fn create_project(parent: &Path, name: &str, workspace: &str) -> Result<PathBuf> {
    // Both halves land inside a quoted TOML string, so an unchecked value can
    // close the quote and append keys of its own. The name comes from the
    // prompt, which already rejects it; `--workspace` reaches here verbatim and
    // is checked at the same boundary, before anything is written.
    for (label, value) in [("project name", name), ("workspace", workspace)] {
        if !valid_scope_name(value) {
            bail!(
                "invalid {label} {value:?}: lowercase letters, digits, dot,                  dash and underscore only, starting with a letter or digit"
            );
        }
    }
    let path = parent.join(name);
    // `create_dir` rather than `create_dir_all`: it fails when the directory
    // already exists, which is the check that keeps a mistyped name from
    // adopting an unrelated tree between the prompt's look and this write.
    std::fs::create_dir(&path)
        .with_context(|| format!("creating project directory {}", path.display()))?;
    apply_atomic(&path.join(".ai-memory.toml"), |_| {
        Ok(marker_body(workspace, name))
    })
    .with_context(|| format!("writing the scope marker in {}", path.display()))?;
    println!("✓ created {}", path.display());
    Ok(path)
}

/// Write the agent context files into the new project: the routing block in
/// the instruction file the chosen harness reads, plus the managed ai-memory
/// Agent Skills that block refers to.
///
/// Delegates to `install-instructions` so a project created here is byte-for-
/// byte what `ai-memory install-instructions` produces, and stays that way as
/// the snippet evolves.
///
/// # Errors
/// Propagates instruction/skill installation failures.
fn install_context_files(config: &Config, harness: RunHarnessChoice, path: &Path) -> Result<()> {
    install_instructions::run(
        config,
        InstallInstructionsArgs {
            target: Some(path.join(instruction_file(harness))),
            print: false,
            no_skills: false,
            skills_scope: None,
            skills_agent: None,
            skills_target_dir: None,
            skills_force: false,
        },
    )
}

/// Projects the server already tracks, newest activity first.
///
/// A listing failure is reported and treated as empty rather than fatal: the
/// scan alone still produces a usable menu, and a first run on a machine whose
/// server is not up yet is exactly when the picker is most useful.
async fn tracked_projects(config: &Config, workspace: Option<&str>) -> Vec<Candidate> {
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let query: Vec<(&str, &str)> = match workspace {
        Some(workspace) if !workspace.is_empty() => vec![("workspace", workspace)],
        _ => Vec::new(),
    };
    let rows: Vec<ProjectRow> = match get_json(&endpoint, "/api/v1/projects", &query).await {
        Ok(rows) => rows,
        Err(error) => {
            eprintln!("ai-memory: listing tracked projects failed ({error:#}); showing scan only");
            return Vec::new();
        }
    };
    rows.into_iter()
        .filter_map(|row| {
            // A row without `repo_path` has no directory to enter — it was
            // created by explicit scope arguments, or its checkout moved.
            let path = row.repo_path.filter(|p| !p.is_empty())?;
            Some(Candidate {
                detail: format!(
                    "{}  ·  {} page{}",
                    humanize_age(row.last_updated.as_deref()),
                    row.page_count,
                    if row.page_count == 1 { "" } else { "s" }
                ),
                workspace: Some(row.workspace_name),
                project: row.project_name,
                path: PathBuf::from(path),
            })
        })
        .collect()
}

/// Immediate subdirectories of `root` that look like projects, plus `root`
/// itself when it is one. Depth 1 only — deep trees are what make a scan slow
/// enough to be noticed, and a workspace directory holds its checkouts flat.
fn scan_for_projects(root: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if is_project_dir(root) {
        found.push(root.to_path_buf());
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // Dot-directories are configuration and caches; the skip list covers
        // dependency and build trees that can each hold thousands of entries.
        if name.starts_with('.') || SKIPPED_DIRS.contains(&name) {
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) && is_project_dir(&path) {
            found.push(path);
        }
    }
    sort_by_recency(&mut found, directory_modified);
    found
}

/// Newest first, then by path.
///
/// The tracked half of the menu is ordered by activity, and the scanned half
/// used to be alphabetical — which buried the project you just created under
/// every checkout whose name sorts earlier. Both halves now answer the same
/// question: what did I touch most recently? Directories whose timestamp
/// cannot be read sort last rather than to the top, and the path tiebreak
/// keeps the order stable when timestamps match.
fn sort_by_recency(paths: &mut [PathBuf], at: impl Fn(&Path) -> Option<SystemTime>) {
    paths.sort_by(|left, right| at(right).cmp(&at(left)).then_with(|| left.cmp(right)));
}

/// Directory mtime, or `None` when the filesystem will not report one.
fn directory_modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// Whether a directory carries any recognised project marker.
fn is_project_dir(path: &std::path::Path) -> bool {
    PROJECT_MARKERS
        .iter()
        .any(|marker| path.join(marker).exists())
}

/// Canonical form for duplicate detection, falling back to the path as given
/// when the filesystem cannot resolve it.
fn normalize(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Directory name as a project name, or `None` for a path without one.
fn directory_name(path: &std::path::Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .map(ToString::to_string)
}

/// Print the start-up banner.
///
/// # Errors
/// Returns an error if stdout cannot be written or flushed.
fn banner() -> Result<()> {
    let mut out = std::io::stdout();
    let art = [
        r"        _        _ __ ___   ___ _ __ ___   ___  _ __ _   _ ",
        r"  __ _ (_)      | '_ ` _ \ / _ \ '_ ` _ \ / _ \| '__| | | |",
        r" / _` || |_____ | | | | | |  __/ | | | | | (_) | |  | |_| |",
        r" \__,_||_|      |_| |_| |_|\___|_| |_| |_|\___/|_|   \__, |",
        r"                                                     |___/ ",
    ];
    writeln!(out)?;
    for line in art {
        writeln!(out, "{}", line.green().bold())?;
    }
    writeln!(
        out,
        "{}",
        "  long-term memory for AI coding agents".dark_green()
    )?;
    writeln!(
        out,
        "{}",
        "  by Fabio Akita · github.com/akitaonrails/ai-memory".dark_green()
    )?;
    writeln!(out)?;
    out.flush()?;
    Ok(())
}

/// Human-readable age of an ISO-8601 timestamp.
fn humanize_age(timestamp: Option<&str>) -> String {
    let Some(raw) = timestamp else {
        return "no pages yet".to_string();
    };
    let Ok(then) = raw.parse::<jiff::Timestamp>() else {
        return raw.to_string();
    };
    let seconds = (jiff::Timestamp::now() - then).get_seconds();
    if seconds < 0 {
        return "just now".to_string();
    }
    let (value, unit) = match seconds {
        s if s < 60 => return "just now".to_string(),
        s if s < 3_600 => (s / 60, "minute"),
        s if s < 86_400 => (s / 3_600, "hour"),
        s if s < 2_592_000 => (s / 86_400, "day"),
        s => (s / 2_592_000, "month"),
    };
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}

/// Display name for a harness variant, taken from the same clap metadata the
/// parser accepts, so the menu can never drift from the CLI surface.
fn harness_name(harness: RunHarnessChoice) -> String {
    harness
        .to_possible_value()
        .map_or_else(|| "unknown".to_string(), |v| v.get_name().to_string())
}

/// Restores raw mode and cursor visibility even when the caller bails early.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort: the process is already unwinding or finishing, and a
        // failed restore must not mask the original error.
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), cursor::Show);
    }
}

/// Draw an arrow-key menu and return the chosen index, or `None` when the
/// user cancels with `Esc`, `q`, or `Ctrl-C`.
///
/// # Errors
/// Returns an error when stdout/stdin is not a terminal, or when raw mode
/// cannot be entered.
fn select(title: &str, choices: &[Choice]) -> Result<Option<usize>> {
    if choices.is_empty() {
        bail!("nothing to choose from");
    }
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        bail!(
            "`ai-memory show` needs an interactive terminal. Use \
             `ai-memory run <harness>` from the project directory in scripts \
             and pipelines"
        );
    }

    terminal::enable_raw_mode().context("entering raw mode")?;
    let _guard = TerminalGuard;
    execute!(std::io::stdout(), cursor::Hide)?;

    // Windows reports a release event for every press. The previous menu's
    // Enter release is still queued when this one opens, and reading it here
    // would select the first entry before the user sees the list. Discard
    // whatever is already pending before taking input.
    while event::poll(std::time::Duration::from_millis(0))? {
        let _ = event::read()?;
    }

    let mut cursor_at = 0usize;
    let mut offset = 0usize;
    let mut drawn = 0u16;
    loop {
        drawn = draw(title, choices, cursor_at, &mut offset, drawn)?;
        // Only key presses matter. Releases and repeats are what made every
        // arrow move two entries on Windows; resize and mouse events fall
        // through and redraw on the next iteration.
        let Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            ..
        }) = event::read()?
        else {
            continue;
        };
        match code {
            KeyCode::Up | KeyCode::Char('k') => {
                cursor_at = cursor_at.checked_sub(1).unwrap_or(choices.len() - 1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                cursor_at = (cursor_at + 1) % choices.len();
            }
            KeyCode::Home => cursor_at = 0,
            KeyCode::End => cursor_at = choices.len() - 1,
            KeyCode::PageUp => cursor_at = cursor_at.saturating_sub(viewport_rows()),
            KeyCode::PageDown => {
                cursor_at = (cursor_at + viewport_rows()).min(choices.len() - 1);
            }
            KeyCode::Enter => {
                clear(drawn)?;
                return Ok(Some(cursor_at));
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                clear(drawn)?;
                return Ok(None);
            }
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                clear(drawn)?;
                return Ok(None);
            }
            _ => {}
        }
    }
}

/// Ask for the new project's name, returning `None` when the user cancels
/// with `Esc` or `Ctrl-C`.
///
/// Rejections are shown in place and leave the typed text alone: the name is
/// validated against the same rule the server enforces, and against `parent`,
/// so every reason the creation could fail is answered while it can still be
/// corrected by typing.
///
/// # Errors
/// Returns an error when raw mode cannot be entered or the terminal cannot be
/// written.
fn prompt_project_name(parent: &Path) -> Result<Option<String>> {
    terminal::enable_raw_mode().context("entering raw mode")?;
    let _guard = TerminalGuard;
    execute!(std::io::stdout(), cursor::Hide)?;
    while event::poll(std::time::Duration::from_millis(0))? {
        let _ = event::read()?;
    }

    let mut name = String::new();
    let mut problem: Option<String> = None;
    let mut drawn = 0u16;
    loop {
        drawn = draw_prompt("New project name", &name, problem.as_deref(), drawn)?;
        let Event::Key(KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            ..
        }) = event::read()?
        else {
            continue;
        };
        match code {
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => {
                clear(drawn)?;
                return Ok(None);
            }
            KeyCode::Esc => {
                clear(drawn)?;
                return Ok(None);
            }
            KeyCode::Backspace => {
                name.pop();
                problem = None;
            }
            KeyCode::Enter => {
                problem = rejection(parent, &name);
                if problem.is_none() {
                    clear(drawn)?;
                    return Ok(Some(name));
                }
            }
            // Control-modified keys are shortcuts, not text. Everything the
            // rule rejects is still accepted into the buffer so the reason
            // shows up under the line the user is looking at.
            KeyCode::Char(c) if !modifiers.contains(KeyModifiers::CONTROL) => {
                name.push(c);
                problem = None;
            }
            _ => {}
        }
    }
}

/// Why `name` cannot be created under `parent`, or `None` when it can.
fn rejection(parent: &Path, name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("type a name".to_string());
    }
    if !valid_scope_name(name) {
        return Some(
            "lowercase letters, digits, dot, dash and underscore only, \
             starting with a letter or digit"
                .to_string(),
        );
    }
    if parent.join(name).exists() {
        return Some(format!("{name} already exists here"));
    }
    None
}

/// Render the name prompt in place and return how many lines it occupies.
fn draw_prompt(title: &str, name: &str, problem: Option<&str>, previous: u16) -> Result<u16> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let width = usize::from(cols.max(20)).saturating_sub(1);

    let mut out = std::io::stdout();
    if previous > 0 {
        execute!(out, cursor::MoveUp(previous))?;
    }
    write!(out, "\r")?;
    execute!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;
    write!(out, "  {}\r\n", truncate(title, width).green().bold())?;
    // A block stands in for the cursor, which stays hidden so the redraw
    // cannot leave it parked on a line it already erased.
    write!(
        out,
        "  > {}{}\r\n",
        truncate(name, width.saturating_sub(5)).as_str().bold(),
        "▏".green()
    )?;
    let footer = match problem {
        Some(problem) => format!("  {problem}"),
        None => "  enter create · esc cancel".to_string(),
    };
    let footer = truncate(&footer, width);
    match problem {
        Some(_) => write!(out, "{}\r\n", footer.as_str().red())?,
        None => write!(out, "{}\r\n", footer.as_str().dark_grey())?,
    }
    out.flush()?;

    Ok(3)
}

/// How many entries fit on screen at once.
///
/// The redraw walks the cursor back up over the block it just printed, which
/// only lands on the right line while the whole block fits in the window. A
/// list taller than the terminal scrolls the top away and every later redraw
/// then targets the wrong row — which is what made a long project list flash
/// and disappear.
fn viewport_rows() -> usize {
    let (_, rows) = terminal::size().unwrap_or((80, 24));
    // Title, hint, and one row of slack for the line the cursor rests on.
    usize::from(rows).saturating_sub(3).max(3)
}

/// First visible entry that keeps `cursor_at` inside a `viewport`-tall window,
/// moving `current` as little as possible so the list does not jump around
/// while the cursor travels within the window.
fn scroll_offset(cursor_at: usize, total: usize, viewport: usize, current: usize) -> usize {
    let mut offset = current;
    if cursor_at < offset {
        offset = cursor_at;
    } else if cursor_at >= offset + viewport {
        offset = cursor_at + 1 - viewport;
    }
    offset.min(total.saturating_sub(viewport))
}

/// Shorten to `width` characters, marking the cut with an ellipsis. Keeps each
/// rendered line inside the terminal so nothing wraps onto a second row and
/// desynchronises the redraw's line count.
fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Render the menu in place and return how many lines it occupies.
///
/// `offset` is the first visible entry, carried between draws so the window
/// only moves when the cursor would leave it.
fn draw(
    title: &str,
    choices: &[Choice],
    cursor_at: usize,
    offset: &mut usize,
    previous: u16,
) -> Result<u16> {
    let (cols, _) = terminal::size().unwrap_or((80, 24));
    let width = usize::from(cols.max(20)).saturating_sub(1);
    let viewport = viewport_rows().min(choices.len());

    *offset = scroll_offset(cursor_at, choices.len(), viewport, *offset);

    let mut out = std::io::stdout();
    if previous > 0 {
        execute!(out, cursor::MoveUp(previous))?;
    }
    // Raw mode disables the implicit carriage return, so every line needs an
    // explicit `\r`.
    write!(out, "\r")?;
    execute!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;
    write!(out, "  {}\r\n", truncate(title, width).green().bold())?;

    for (index, choice) in choices.iter().enumerate().skip(*offset).take(viewport) {
        let marker = if index == cursor_at { "  > " } else { "    " };
        let label = truncate(&choice.label, width.saturating_sub(marker.len()));
        let used = marker.len() + label.chars().count();
        if index == cursor_at {
            write!(
                out,
                "{}{}",
                marker.green().bold(),
                label.as_str().green().bold()
            )?;
        } else {
            write!(out, "{marker}{}", label.as_str().reset())?;
        }
        if !choice.detail.is_empty() && used + 2 < width {
            let detail = truncate(&choice.detail, width - used - 2);
            write!(out, "  {}", detail.as_str().dark_grey())?;
        }
        write!(out, "\r\n")?;
    }

    let hint = if choices.len() > viewport {
        format!(
            "  ↑/↓ move · pgup/pgdn page · enter select · esc cancel   [{}/{}]",
            cursor_at + 1,
            choices.len()
        )
    } else {
        "  ↑/↓ move · enter select · esc cancel".to_string()
    };
    write!(out, "{}\r\n", truncate(&hint, width).dark_grey())?;
    out.flush()?;

    // title + visible entries + hint
    Ok(u16::try_from(viewport)
        .unwrap_or(u16::MAX)
        .saturating_add(2))
}

/// Erase the menu block so the launched agent starts on a clean screen.
fn clear(drawn: u16) -> Result<()> {
    let mut out = std::io::stdout();
    if drawn > 0 {
        execute!(out, cursor::MoveUp(drawn))?;
    }
    execute!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;
    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_age_reports_missing_pages() {
        assert_eq!(humanize_age(None), "no pages yet");
    }

    #[test]
    fn humanize_age_passes_through_unparseable_timestamps() {
        assert_eq!(humanize_age(Some("not-a-timestamp")), "not-a-timestamp");
    }

    #[test]
    fn humanize_age_singularises_and_pluralises() {
        let two_days = jiff::Timestamp::now() - std::time::Duration::from_secs(2 * 86_400);
        assert_eq!(humanize_age(Some(&two_days.to_string())), "2 days ago");

        let one_hour = jiff::Timestamp::now() - std::time::Duration::from_secs(3_600);
        assert_eq!(humanize_age(Some(&one_hour.to_string())), "1 hour ago");
    }

    /// A clock skew that puts a page in the future must not underflow into a
    /// nonsensical age.
    #[test]
    fn humanize_age_clamps_future_timestamps() {
        let ahead = jiff::Timestamp::now() + std::time::Duration::from_secs(600);
        assert_eq!(humanize_age(Some(&ahead.to_string())), "just now");
    }

    /// The menu labels come from clap's own metadata, so every harness the
    /// parser accepts can be named by the picker.
    #[test]
    fn harness_names_cover_every_parser_variant() {
        for harness in RunHarnessChoice::value_variants() {
            assert_ne!(harness_name(*harness), "unknown");
        }
    }

    /// Availability must not panic or depend on the working directory: the
    /// picker calls it for every variant before drawing the menu, from
    /// whatever directory the user happened to be in.
    #[test]
    fn availability_is_answerable_for_every_variant() {
        let tmp = tempfile::TempDir::new().unwrap();
        let restore = std::env::current_dir().unwrap();
        std::env::set_current_dir(tmp.path()).unwrap();

        let answers: Vec<bool> = RunHarnessChoice::value_variants()
            .iter()
            .map(|h| crate::commands::run::harness_available(*h))
            .collect();

        std::env::set_current_dir(restore).unwrap();
        assert_eq!(answers.len(), RunHarnessChoice::value_variants().len());
    }

    #[test]
    fn selecting_from_an_empty_list_is_an_error() {
        assert!(select("empty", &[]).is_err());
    }

    /// The redraw walks back up over exactly the block it printed, so the
    /// window must never expose more entries than fit — a taller block scrolls
    /// the top away and every later redraw targets the wrong row.
    #[test]
    fn scroll_offset_keeps_the_cursor_inside_the_window() {
        // Cursor within the current window leaves it untouched.
        assert_eq!(scroll_offset(2, 24, 5, 0), 0);
        // Falling off the bottom scrolls by the minimum.
        assert_eq!(scroll_offset(5, 24, 5, 0), 1);
        assert_eq!(scroll_offset(6, 24, 5, 1), 2);
        // Falling off the top scrolls back up to the cursor.
        assert_eq!(scroll_offset(3, 24, 5, 7), 3);
        // The window never runs past the end of the list.
        assert_eq!(scroll_offset(23, 24, 5, 22), 19);
    }

    #[test]
    fn scroll_offset_is_zero_when_everything_fits() {
        for cursor in 0..5 {
            assert_eq!(scroll_offset(cursor, 5, 10, 0), 0);
        }
    }

    #[test]
    fn truncate_marks_the_cut_and_respects_the_width() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("exactfit", 8), "exactfit");
        assert_eq!(truncate("truncate-me", 5), "trun…");
        assert_eq!(truncate("anything", 0), "");
    }

    /// A wrapped line would occupy two rows and desynchronise the line count
    /// the redraw walks back over, so nothing may exceed the given width.
    #[test]
    fn truncate_never_exceeds_the_requested_width() {
        for width in 0..12 {
            assert!(
                truncate("a considerably longer label", width)
                    .chars()
                    .count()
                    <= width
            );
        }
    }

    /// Create `dir` and drop `marker` inside it when one is given.
    fn make_dir(root: &std::path::Path, name: &str, marker: Option<&str>) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(marker) = marker {
            std::fs::write(dir.join(marker), "").unwrap();
        }
        dir
    }

    #[test]
    fn scan_finds_marked_subdirectories_and_ignores_plain_ones() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_dir(tmp.path(), "with-git", Some(".git"));
        make_dir(tmp.path(), "with-cargo", Some("Cargo.toml"));
        make_dir(tmp.path(), "just-a-folder", None);

        let found = scan_for_projects(tmp.path());
        let names: Vec<String> = found.iter().filter_map(|p| directory_name(p)).collect();

        assert!(names.contains(&"with-git".to_string()));
        assert!(names.contains(&"with-cargo".to_string()));
        assert!(
            !names.contains(&"just-a-folder".to_string()),
            "a directory with no marker is not a project"
        );
    }

    /// Dependency and build trees can hold thousands of entries and are never
    /// projects; descending into them is what would make the scan noticeable.
    #[test]
    fn scan_skips_dependency_build_and_dot_directories() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_dir(tmp.path(), "node_modules", Some("package.json"));
        make_dir(tmp.path(), "target", Some("Cargo.toml"));
        make_dir(tmp.path(), ".cache", Some("package.json"));

        assert!(scan_for_projects(tmp.path()).is_empty());
    }

    /// Running the picker from inside a single checkout should still offer
    /// that checkout, not just its children.
    #[test]
    fn scan_includes_the_root_when_the_root_is_itself_a_project() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "").unwrap();

        let found = scan_for_projects(tmp.path());

        assert_eq!(found.len(), 1);
        assert_eq!(normalize(&found[0]), normalize(tmp.path()));
    }

    #[test]
    fn scan_of_an_unreadable_root_is_empty_rather_than_an_error() {
        let missing = std::path::Path::new("definitely-not-a-real-directory-xyz");
        assert!(scan_for_projects(missing).is_empty());
    }

    /// The rule the server enforces at `get_or_create_project`. A name that
    /// passes here and fails there would only surface as a hook warning on a
    /// session that already started.
    #[test]
    fn scope_names_follow_the_server_rule() {
        for ok in ["ai-memory", "app2", "my_project", "a", "web.api", "0day"] {
            assert!(valid_scope_name(ok), "{ok} should be accepted");
        }
        for bad in [
            "",
            "-leading-dash",
            ".dotfile",
            "_underscore",
            "UpperCase",
            "with space",
            "acentuação",
            "sub/dir",
        ] {
            assert!(!valid_scope_name(bad), "{bad} should be rejected");
        }
    }

    /// Every reason creation could fail is answered while the name can still
    /// be corrected by typing.
    #[test]
    fn rejection_explains_empty_invalid_and_taken_names() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_dir(tmp.path(), "taken", None);

        assert_eq!(rejection(tmp.path(), "").as_deref(), Some("type a name"));
        assert!(
            rejection(tmp.path(), "Bad Name").is_some_and(|p| p.contains("lowercase")),
            "an invalid name names the rule"
        );
        assert!(
            rejection(tmp.path(), "taken").is_some_and(|p| p.contains("already exists")),
            "an occupied directory is refused before it can be adopted"
        );
        assert_eq!(rejection(tmp.path(), "brand-new"), None);
    }

    /// Both scope halves are pinned so a rename or a subdirectory `cd` cannot
    /// fork the memory into a second, empty project.
    #[test]
    fn created_marker_pins_both_scope_halves() {
        let body = marker_body("acme", "portal");
        assert!(body.contains("workspace = \"acme\""));
        assert!(body.contains("project = \"portal\""));
    }

    #[test]
    fn creating_a_project_writes_the_directory_and_its_marker() {
        let tmp = tempfile::TempDir::new().unwrap();

        let path = create_project(tmp.path(), "fresh", "default").unwrap();

        assert!(path.is_dir());
        let marker = std::fs::read_to_string(path.join(".ai-memory.toml")).unwrap();
        assert!(marker.contains("project = \"fresh\""));
        // The marker is itself a recognised marker, so the project shows up in
        // the scan on the next run even before the server has tracked it.
        assert!(is_project_dir(&path));
    }

    /// An existing directory must not be adopted: it can hold an unrelated
    /// project whose own marker the creation would overwrite.
    #[test]
    fn creating_over_an_existing_directory_fails() {
        let tmp = tempfile::TempDir::new().unwrap();
        make_dir(tmp.path(), "occupied", Some("Cargo.toml"));

        assert!(create_project(tmp.path(), "occupied", "default").is_err());
    }

    /// The routing block only takes effect in the file the chosen agent reads.
    #[test]
    fn instruction_file_matches_the_harness_convention() {
        assert_eq!(instruction_file(RunHarnessChoice::Claude), "CLAUDE.md");
        for other in RunHarnessChoice::value_variants()
            .iter()
            .filter(|h| !matches!(h, RunHarnessChoice::Claude))
        {
            assert_eq!(instruction_file(*other), "AGENTS.md");
        }
    }

    /// `--workspace` reaches the marker verbatim, so an unvalidated value can
    /// close the TOML string and append keys of its own. The name typed into
    /// the prompt is checked; this argument must be held to the same rule.
    #[test]
    fn workspace_names_are_validated_before_reaching_the_marker() {
        let tmp = tempfile::TempDir::new().unwrap();

        let injected = "default\"
project = \"hijacked";
        let error = create_project(tmp.path(), "victim", injected).unwrap_err();

        assert!(
            error.to_string().contains("workspace"),
            "the failure must name the offending argument: {error}"
        );
        assert!(
            !tmp.path().join("victim").exists(),
            "nothing may be created when the scope is invalid"
        );
    }

    /// The project you just created has to be near the top: alphabetical order
    /// buried it under every checkout whose name sorts earlier, which is the
    /// one case the picker exists to serve.
    #[test]
    fn scanned_projects_are_ordered_newest_first() {
        let base = SystemTime::UNIX_EPOCH;
        let mut paths: Vec<PathBuf> = ["alpha", "personal", "zulu", "unreadable"]
            .iter()
            .map(PathBuf::from)
            .collect();

        sort_by_recency(&mut paths, |path| {
            match path.to_str().unwrap() {
                "alpha" => Some(base + std::time::Duration::from_secs(10)),
                "personal" => Some(base + std::time::Duration::from_secs(90)),
                "zulu" => Some(base + std::time::Duration::from_secs(50)),
                // A directory whose timestamp cannot be read must not win the
                // top slot by default.
                _ => None,
            }
        });

        assert_eq!(
            paths
                .iter()
                .map(|p| p.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["personal", "zulu", "alpha", "unreadable"]
        );
    }

    /// Equal timestamps must not reorder between runs.
    #[test]
    fn equal_timestamps_fall_back_to_a_stable_path_order() {
        let mut paths: Vec<PathBuf> = ["b", "c", "a"].iter().map(PathBuf::from).collect();
        sort_by_recency(&mut paths, |_| Some(SystemTime::UNIX_EPOCH));
        assert_eq!(
            paths
                .iter()
                .map(|p| p.to_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
    }

    /// The same checkout reached by two spellings must dedupe, otherwise a
    /// tracked project would also appear as an untracked scan hit.
    #[test]
    fn normalize_matches_equivalent_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let nested = tmp.path().join("child");
        std::fs::create_dir_all(&nested).unwrap();

        let indirect = tmp.path().join("child").join("..").join("child");

        assert_eq!(normalize(&nested), normalize(&indirect));
    }
}
