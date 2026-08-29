//! `ai-memory show` project and managed-harness picker.
//!
//! Remote project metadata is joined with a client-local checkout registry.
//! The server never sends host filesystem paths, and each client may map the
//! same remote scope to a different local checkout.

use std::collections::HashMap;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::ValueEnum as _;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Stylize as _;
use crossterm::{cursor, execute, terminal};
use serde::{Deserialize, Serialize};

use crate::cli::{
    InstallInstructionsArgs, InstallSkillsAgent, RunArgs, RunHarnessChoice, ShowArgs,
};
use crate::commands::{install_instructions, project_registry};
use crate::config::{Config, DEFAULT_WORKSPACE};
use crate::http_client::{ServerEndpoint, get_json};

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
const MAX_SCAN_ENTRIES: usize = 4096;
const NEW_PROJECT_LABEL: &str = "+ New project";

#[derive(Debug, Clone, Deserialize)]
struct ProjectRow {
    workspace_name: String,
    project_name: String,
    page_count: u64,
    last_updated: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateSource {
    Registry,
    Scan,
}

impl CandidateSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Registry => "registry",
            Self::Scan => "scan",
        }
    }
}

#[derive(Debug, Clone)]
struct Candidate {
    workspace: String,
    project: String,
    path: PathBuf,
    source: CandidateSource,
    page_count: Option<u64>,
    last_updated: Option<String>,
    activity_us: Option<i64>,
}

impl Candidate {
    fn label(&self) -> String {
        terminal_text(&format!("{}/{}", self.workspace, self.project))
    }

    fn detail(&self) -> String {
        let path = terminal_text(&self.path.to_string_lossy());
        match self.page_count {
            Some(count) => format!(
                "{} | {count} page{} | {path}",
                humanize_age(self.last_updated.as_deref()),
                if count == 1 { "" } else { "s" }
            ),
            None => format!("local only | {path}"),
        }
    }
}

struct Choice {
    label: String,
    detail: String,
}

#[derive(Serialize)]
struct JsonProject {
    workspace: String,
    project: String,
    path: String,
    source: &'static str,
    tracked: bool,
    page_count: Option<u64>,
    last_updated: Option<String>,
}

#[derive(Serialize)]
struct JsonOutput {
    server: String,
    projects: Vec<JsonProject>,
    harnesses: Vec<String>,
}

/// Pick a local checkout and harness, then delegate to managed `run`.
pub async fn run(config: &Config, args: ShowArgs) -> Result<i32> {
    if args.json && (args.yolo || args.fresh || !args.native_args.is_empty()) {
        bail!("--json only lists launch options; do not combine it with launch arguments");
    }
    let root = std::env::current_dir()
        .context("reading the current directory")?
        .canonicalize()
        .context("canonicalizing the current directory")?;
    let endpoint = ServerEndpoint::from_config_resolving_auth(config).await;
    let candidates = collect_candidates(
        config,
        &endpoint,
        &root,
        args.workspace.as_deref(),
        !args.no_scan,
    )
    .await;
    let harnesses = available_harnesses();

    if args.json {
        print_json(&endpoint, &candidates, &harnesses)?;
        return Ok(0);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "`ai-memory show` needs a terminal; use `ai-memory show --json` to list choices in scripts"
        );
    }
    if harnesses.is_empty() {
        bail!(
            "no supported managed harness was found in PATH (looked for: {})",
            RunHarnessChoice::value_variants()
                .iter()
                .map(|choice| harness_name(*choice))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    let mut project_choices = vec![Choice {
        label: NEW_PROJECT_LABEL.to_owned(),
        detail: "create a directory and install its agent context".to_owned(),
    }];
    project_choices.extend(candidates.iter().map(|candidate| Choice {
        label: candidate.label(),
        detail: candidate.detail(),
    }));
    let Some(project_index) = select("Project", &project_choices)? else {
        return Ok(0);
    };
    let new_project = if project_index == 0 {
        let Some(name) = prompt_project_name(&root)? else {
            return Ok(0);
        };
        Some(name)
    } else {
        None
    };

    let harness_choices = harnesses
        .iter()
        .map(|choice| Choice {
            label: harness_name(*choice),
            detail: String::new(),
        })
        .collect::<Vec<_>>();
    let project_label = new_project
        .as_deref()
        .map(terminal_text)
        .unwrap_or_else(|| project_choices[project_index].label.clone());
    let Some(harness_index) = select(&format!("Agent | {project_label}"), &harness_choices)? else {
        return Ok(0);
    };
    let harness = harnesses[harness_index];

    let (target, workspace, project) = if let Some(name) = new_project {
        let workspace = args.workspace.as_deref().unwrap_or(DEFAULT_WORKSPACE);
        let target = create_project(&root, &name, workspace, harness)?;
        (target, workspace.to_owned(), name)
    } else {
        let candidate = &candidates[project_index - 1];
        let target = revalidate_candidate(config, candidate)?;
        (
            target,
            candidate.workspace.clone(),
            candidate.project.clone(),
        )
    };

    crate::commands::run::run_from(
        config,
        RunArgs {
            workspace: Some(workspace),
            project: Some(project),
            workstream: None,
            new_workstream: None,
            executable: None,
            yolo: args.yolo,
            fresh: args.fresh,
            harness: Some(harness),
            native_args: args.native_args,
        },
        &target,
    )
    .await
}

async fn collect_candidates(
    config: &Config,
    endpoint: &ServerEndpoint,
    root: &Path,
    workspace_filter: Option<&str>,
    scan: bool,
) -> Vec<Candidate> {
    let query = workspace_filter
        .filter(|value| !value.is_empty())
        .map_or_else(Vec::new, |workspace| vec![("workspace", workspace)]);
    let rows: Option<Vec<ProjectRow>> = match get_json(endpoint, "/api/v1/projects", &query).await {
        Ok(rows) => Some(rows),
        Err(error) => {
            eprintln!(
                "ai-memory: server project listing unavailable ({}); using client-local links and scan results",
                terminal_text(&format!("{error:#}"))
            );
            None
        }
    };
    let stats = rows.as_ref().map(|rows| {
        rows.iter()
            .cloned()
            .map(|row| ((row.workspace_name.clone(), row.project_name.clone()), row))
            .collect::<HashMap<_, _>>()
    });

    let links = match project_registry::links_for_server(config, endpoint) {
        Ok(links) => links,
        Err(error) => {
            eprintln!(
                "ai-memory: client project registry unavailable ({}); using scan results",
                terminal_text(&format!("{error:#}"))
            );
            Vec::new()
        }
    };
    let mut candidates = Vec::new();
    let mut stale_warnings = 0usize;
    for link in links {
        if workspace_filter.is_some_and(|filter| filter != link.workspace) {
            continue;
        }
        let key = (link.workspace.clone(), link.project.clone());
        if stats
            .as_ref()
            .is_some_and(|known| !known.contains_key(&key))
        {
            continue;
        }
        let path = match validate_registered_path(&link.path) {
            Ok(path) => path,
            Err(error) => {
                if stale_warnings < 3 {
                    eprintln!(
                        "ai-memory: ignored stale client project link {}/{} ({})",
                        terminal_text(&link.workspace),
                        terminal_text(&link.project),
                        terminal_text(&format!("{error:#}"))
                    );
                }
                stale_warnings += 1;
                continue;
            }
        };
        let row = stats.as_ref().and_then(|known| known.get(&key));
        push_candidate(
            &mut candidates,
            candidate_from(
                path,
                link.workspace,
                link.project,
                CandidateSource::Registry,
                row,
                Some(&link.linked_at),
            ),
        );
    }
    if stale_warnings > 3 {
        eprintln!(
            "ai-memory: ignored {} additional stale client project links",
            stale_warnings - 3
        );
    }

    if scan {
        for path in scan_for_projects(root) {
            let Ok((workspace, project)) = super::resolve_scope_for_path(config, &path) else {
                continue;
            };
            if workspace_filter.is_some_and(|filter| filter != workspace) {
                continue;
            }
            let key = (workspace.clone(), project.clone());
            let row = stats.as_ref().and_then(|known| known.get(&key));
            push_candidate(
                &mut candidates,
                candidate_from(path, workspace, project, CandidateSource::Scan, row, None),
            );
        }
    }
    candidates.sort_by(|left, right| {
        right
            .activity_us
            .cmp(&left.activity_us)
            .then_with(|| left.workspace.cmp(&right.workspace))
            .then_with(|| left.project.cmp(&right.project))
            .then_with(|| left.path.cmp(&right.path))
    });
    candidates
}

fn candidate_from(
    path: PathBuf,
    workspace: String,
    project: String,
    source: CandidateSource,
    row: Option<&ProjectRow>,
    local_activity: Option<&str>,
) -> Candidate {
    Candidate {
        workspace,
        project,
        activity_us: row
            .and_then(|row| timestamp_us(row.last_updated.as_deref()))
            .or_else(|| timestamp_us(local_activity))
            .or_else(|| directory_modified_us(&path)),
        page_count: row.map(|row| row.page_count),
        last_updated: row.and_then(|row| row.last_updated.clone()),
        path,
        source,
    }
}

fn push_candidate(candidates: &mut Vec<Candidate>, candidate: Candidate) {
    if let Some(existing) = candidates
        .iter_mut()
        .find(|existing| existing.path == candidate.path)
    {
        if existing.page_count.is_none() && candidate.page_count.is_some() {
            existing.page_count = candidate.page_count;
            existing.last_updated = candidate.last_updated;
            existing.activity_us = candidate.activity_us;
        }
        return;
    }
    candidates.push(candidate);
}

fn print_json(
    endpoint: &ServerEndpoint,
    candidates: &[Candidate],
    harnesses: &[RunHarnessChoice],
) -> Result<()> {
    let output = JsonOutput {
        server: endpoint.identity(),
        projects: candidates
            .iter()
            .map(|candidate| JsonProject {
                workspace: candidate.workspace.clone(),
                project: candidate.project.clone(),
                path: candidate.path.to_string_lossy().into_owned(),
                source: candidate.source.as_str(),
                tracked: candidate.page_count.is_some(),
                page_count: candidate.page_count,
                last_updated: candidate.last_updated.clone(),
            })
            .collect(),
        harnesses: harnesses
            .iter()
            .map(|choice| harness_name(*choice))
            .collect(),
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer_pretty(&mut out, &output).context("rendering project list JSON")?;
    writeln!(out)?;
    Ok(())
}

fn available_harnesses() -> Vec<RunHarnessChoice> {
    RunHarnessChoice::value_variants()
        .iter()
        .copied()
        .filter(|choice| crate::commands::run::harness_available(*choice))
        .collect()
}

/// Re-check a stored checkout path immediately before it is used.
///
/// The canonical-equality check is the load-bearing one: it rejects a
/// recorded directory that has since been replaced by a symlink pointing
/// somewhere else, before any harness is launched inside it.
pub(super) fn validate_registered_path(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        bail!("stored checkout path is not absolute");
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("checkout {} no longer resolves", path.display()))?;
    if canonical != path {
        bail!("checkout path now resolves somewhere else");
    }
    if !canonical.is_dir() {
        bail!("checkout path is not a directory");
    }
    Ok(canonical)
}

fn revalidate_candidate(config: &Config, candidate: &Candidate) -> Result<PathBuf> {
    let path = validate_registered_path(&candidate.path)?;
    let (workspace, project) = super::resolve_scope_for_path(config, &path)?;
    if workspace != candidate.workspace || project != candidate.project {
        bail!(
            "checkout scope changed after it was listed: expected {}/{}, found {}/{}; rerun `ai-memory show`",
            terminal_text(&candidate.workspace),
            terminal_text(&candidate.project),
            terminal_text(&workspace),
            terminal_text(&project)
        );
    }
    Ok(path)
}

fn scan_for_projects(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    if is_project_dir(root) {
        found.push(root.to_path_buf());
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return found;
    };
    for entry in entries.take(MAX_SCAN_ENTRIES).flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || SKIPPED_DIRS.contains(&name) {
            continue;
        }
        if entry.file_type().is_ok_and(|kind| kind.is_dir())
            && is_project_dir(&path)
            && let Ok(canonical) = path.canonicalize()
        {
            found.push(canonical);
        }
    }
    found.sort_by(|left, right| {
        directory_modified_us(right)
            .cmp(&directory_modified_us(left))
            .then_with(|| left.cmp(right))
    });
    found
}

fn is_project_dir(path: &Path) -> bool {
    PROJECT_MARKERS
        .iter()
        .any(|marker| path.join(marker).exists())
}

fn directory_modified_us(path: &Path) -> Option<i64> {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(system_time_us)
}

fn system_time_us(time: SystemTime) -> Option<i64> {
    let micros = time.duration_since(UNIX_EPOCH).ok()?.as_micros();
    i64::try_from(micros).ok()
}

fn timestamp_us(raw: Option<&str>) -> Option<i64> {
    raw?.parse::<jiff::Timestamp>()
        .ok()
        .map(|ts| ts.as_microsecond())
}

fn portable_component(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '-' | '_')
        })
}

fn marker_body(workspace: &str, project: &str) -> String {
    let mut document = toml_edit::DocumentMut::new();
    document["workspace"] = toml_edit::value(workspace);
    document["project"] = toml_edit::value(project);
    format!(
        "# Created by `ai-memory show`. Both scope names are pinned so moving\n\
         # into a subdirectory or renaming this checkout does not fork memory.\n\
         {document}"
    )
}

fn create_project(
    parent: &Path,
    name: &str,
    workspace: &str,
    harness: RunHarnessChoice,
) -> Result<PathBuf> {
    create_project_with_initializer(parent, name, workspace, |staging| {
        install_context_files(harness, staging)
    })
}

fn create_project_with_initializer(
    parent: &Path,
    name: &str,
    workspace: &str,
    initialize: impl FnOnce(&Path) -> Result<()>,
) -> Result<PathBuf> {
    if !portable_component(name) {
        bail!(
            "invalid project name {name:?}: use lowercase ASCII letters, digits, dot, dash, or underscore, starting with a letter or digit"
        );
    }
    let target = parent.join(name);
    if target.exists() {
        bail!("project directory already exists: {}", target.display());
    }
    let staging = tempfile::Builder::new()
        .prefix(".ai-memory-project-")
        .tempdir_in(parent)
        .with_context(|| format!("creating a staging directory in {}", parent.display()))?;
    ai_memory_wiki::write_atomic(
        &staging.path().join(".ai-memory.toml"),
        marker_body(workspace, name).as_bytes(),
    )
    .context("writing the staged project marker")?;
    initialize(staging.path())?;
    if target.exists() {
        bail!(
            "project directory appeared while it was being prepared: {}",
            target.display()
        );
    }
    std::fs::rename(staging.path(), &target)
        .with_context(|| format!("publishing new project {}", target.display()))?;
    let target = target
        .canonicalize()
        .with_context(|| format!("canonicalizing new project {}", target.display()))?;
    println!("created {}", target.display());
    Ok(target)
}

fn install_context_files(harness: RunHarnessChoice, path: &Path) -> Result<()> {
    let (instruction, agent, skill_root) = match harness {
        RunHarnessChoice::Claude => (
            "CLAUDE.md",
            InstallSkillsAgent::ClaudeCode,
            path.join(".claude/skills"),
        ),
        _ => (
            "AGENTS.md",
            InstallSkillsAgent::Agents,
            path.join(".agents/skills"),
        ),
    };
    install_instructions::run_quiet(InstallInstructionsArgs {
        target: Some(path.join(instruction)),
        print: false,
        no_skills: false,
        skills_scope: None,
        skills_agent: Some(agent),
        skills_target_dir: Some(skill_root),
        skills_force: false,
    })
}

fn humanize_age(timestamp: Option<&str>) -> String {
    let Some(raw) = timestamp else {
        return "no pages yet".to_owned();
    };
    let Ok(then) = raw.parse::<jiff::Timestamp>() else {
        return "unknown activity".to_owned();
    };
    super::humanize_age_secs((jiff::Timestamp::now() - then).get_seconds())
}

fn harness_name(harness: RunHarnessChoice) -> String {
    harness
        .to_possible_value()
        .map_or_else(|| "unknown".to_owned(), |value| value.get_name().to_owned())
}

pub(super) fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect()
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), cursor::Show);
    }
}

fn select(title: &str, choices: &[Choice]) -> Result<Option<usize>> {
    if choices.is_empty() {
        bail!("nothing to choose from");
    }
    terminal::enable_raw_mode().context("entering raw mode")?;
    let _guard = TerminalGuard;
    execute!(std::io::stdout(), cursor::Hide)?;
    while event::poll(std::time::Duration::from_millis(0))? {
        let _ = event::read()?;
    }

    let mut selected = 0usize;
    let mut offset = 0usize;
    let mut drawn = 0u16;
    loop {
        drawn = draw(title, choices, selected, &mut offset, drawn)?;
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
                selected = selected.checked_sub(1).unwrap_or(choices.len() - 1);
            }
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1) % choices.len(),
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = choices.len() - 1,
            KeyCode::PageUp => selected = selected.saturating_sub(viewport_rows()),
            KeyCode::PageDown => {
                selected = (selected + viewport_rows()).min(choices.len() - 1);
            }
            KeyCode::Enter => {
                clear(drawn)?;
                return Ok(Some(selected));
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

fn prompt_project_name(parent: &Path) -> Result<Option<String>> {
    terminal::enable_raw_mode().context("entering raw mode")?;
    let _guard = TerminalGuard;
    execute!(std::io::stdout(), cursor::Hide)?;
    while event::poll(std::time::Duration::from_millis(0))? {
        let _ = event::read()?;
    }

    let mut name = String::new();
    let mut problem = None;
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
            KeyCode::Char(character) if !modifiers.contains(KeyModifiers::CONTROL) => {
                name.push(character);
                problem = None;
            }
            _ => {}
        }
    }
}

fn rejection(parent: &Path, name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("type a name".to_owned());
    }
    if !portable_component(name) {
        return Some("use lowercase letters, digits, dot, dash, or underscore".to_owned());
    }
    if parent.join(name).exists() {
        return Some(format!("{name} already exists here"));
    }
    None
}

fn draw_prompt(title: &str, name: &str, problem: Option<&str>, previous: u16) -> Result<u16> {
    let (columns, _) = terminal::size().unwrap_or((80, 24));
    let width = usize::from(columns.max(20)).saturating_sub(1);
    let mut out = std::io::stdout();
    if previous > 0 {
        execute!(out, cursor::MoveUp(previous))?;
    }
    write!(out, "\r")?;
    execute!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;
    write!(
        out,
        "  {}\r\n",
        truncate(&terminal_text(title), width).green().bold()
    )?;
    write!(
        out,
        "  > {}{}\r\n",
        truncate(&terminal_text(name), width.saturating_sub(5))
            .as_str()
            .bold(),
        "|".green()
    )?;
    let footer = problem
        .map(|problem| format!("  {}", terminal_text(problem)))
        .unwrap_or_else(|| "  enter create | esc cancel".to_owned());
    let footer = truncate(&footer, width);
    if problem.is_some() {
        write!(out, "{}\r\n", footer.as_str().red())?;
    } else {
        write!(out, "{}\r\n", footer.as_str().dark_grey())?;
    }
    out.flush()?;
    Ok(3)
}

fn viewport_rows() -> usize {
    let (_, rows) = terminal::size().unwrap_or((80, 24));
    usize::from(rows).saturating_sub(3).max(3)
}

fn scroll_offset(selected: usize, total: usize, viewport: usize, current: usize) -> usize {
    let mut offset = current;
    if selected < offset {
        offset = selected;
    } else if selected >= offset + viewport {
        offset = selected + 1 - viewport;
    }
    offset.min(total.saturating_sub(viewport))
}

fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut output = text
        .chars()
        .take(width.saturating_sub(1))
        .collect::<String>();
    output.push('…');
    output
}

fn draw(
    title: &str,
    choices: &[Choice],
    selected: usize,
    offset: &mut usize,
    previous: u16,
) -> Result<u16> {
    let (columns, _) = terminal::size().unwrap_or((80, 24));
    let width = usize::from(columns.max(20)).saturating_sub(1);
    let viewport = viewport_rows().min(choices.len());
    *offset = scroll_offset(selected, choices.len(), viewport, *offset);

    let mut out = std::io::stdout();
    if previous > 0 {
        execute!(out, cursor::MoveUp(previous))?;
    }
    write!(out, "\r")?;
    execute!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;
    write!(
        out,
        "  {}\r\n",
        truncate(&terminal_text(title), width).green().bold()
    )?;
    for (index, choice) in choices.iter().enumerate().skip(*offset).take(viewport) {
        let marker = if index == selected { "  > " } else { "    " };
        let label = truncate(
            &terminal_text(&choice.label),
            width.saturating_sub(marker.len()),
        );
        let used = marker.len() + label.chars().count();
        if index == selected {
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
            let detail = truncate(&terminal_text(&choice.detail), width - used - 2);
            write!(out, "  {}", detail.as_str().dark_grey())?;
        }
        write!(out, "\r\n")?;
    }
    let hint = if choices.len() > viewport {
        format!(
            "  up/down move | pgup/pgdn page | enter select | esc cancel [{}/{}]",
            selected + 1,
            choices.len()
        )
    } else {
        "  up/down move | enter select | esc cancel".to_owned()
    };
    write!(out, "{}\r\n", truncate(&hint, width).dark_grey())?;
    out.flush()?;
    Ok(u16::try_from(viewport)
        .unwrap_or(u16::MAX)
        .saturating_add(2))
}

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

    fn config_at(path: &Path) -> Config {
        Config {
            data_dir: path.to_path_buf(),
            ..Config::default()
        }
    }

    fn make_project(root: &Path, name: &str, marker: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join(marker), b"").unwrap();
        path.canonicalize().unwrap()
    }

    #[test]
    fn terminal_text_removes_control_sequences() {
        assert_eq!(terminal_text("safe\n\u{1b}[31mname\r"), "safe??[31mname?");
    }

    #[test]
    fn invalid_server_timestamp_is_not_echoed_to_the_terminal() {
        assert_eq!(humanize_age(Some("\u{1b}[31m")), "unknown activity");
    }

    #[test]
    fn registry_activity_orders_projects_when_server_metadata_is_unavailable() {
        let timestamp = "2026-08-01T12:00:00Z";
        let candidate = candidate_from(
            PathBuf::from("/missing-is-fine-for-this-pure-test"),
            "default".to_owned(),
            "app".to_owned(),
            CandidateSource::Registry,
            None,
            Some(timestamp),
        );

        assert_eq!(candidate.activity_us, timestamp_us(Some(timestamp)));
    }

    #[test]
    fn portable_project_components_reject_injection_and_traversal() {
        for valid in ["ai-memory", "app2", "my_project", "web.api"] {
            assert!(portable_component(valid));
        }
        for invalid in ["", "../other", "Upper", "with space", "x\nproject", "x\""] {
            assert!(!portable_component(invalid));
        }
    }

    #[test]
    fn scan_is_depth_one_and_skips_build_directories() {
        let tmp = tempfile::TempDir::new().unwrap();
        let project = make_project(tmp.path(), "app", "Cargo.toml");
        let nested = tmp.path().join("plain/nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("go.mod"), b"").unwrap();
        make_project(tmp.path(), "target", "Cargo.toml");

        assert_eq!(scan_for_projects(tmp.path()), vec![project]);
    }

    #[test]
    fn failed_project_initialization_rolls_back_the_staging_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let result = create_project_with_initializer(tmp.path(), "app", "default", |_path| {
            bail!("synthetic failure")
        });

        assert!(result.is_err());
        assert!(!tmp.path().join("app").exists());
        assert!(std::fs::read_dir(tmp.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".ai-memory-project-")
        }));
    }

    #[test]
    fn created_project_is_published_only_after_initialization() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = create_project_with_initializer(tmp.path(), "app", "default", |staging| {
            assert!(!tmp.path().join("app").exists());
            std::fs::write(staging.join("AGENTS.md"), b"ready")?;
            Ok(())
        })
        .unwrap();

        assert_eq!(std::fs::read(path.join("AGENTS.md")).unwrap(), b"ready");
        let marker = std::fs::read_to_string(path.join(".ai-memory.toml")).unwrap();
        assert!(marker.contains("workspace = \"default\""));
        assert!(marker.contains("project = \"app\""));
    }

    #[test]
    fn marker_encoding_preserves_an_existing_nonportable_workspace_name() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path =
            create_project_with_initializer(tmp.path(), "app", "Team \"One\"", |_staging| Ok(()))
                .unwrap();

        let marker = std::fs::read_to_string(path.join(".ai-memory.toml")).unwrap();
        let document = marker.parse::<toml_edit::DocumentMut>().unwrap();
        assert_eq!(document["workspace"].as_str(), Some("Team \"One\""));
    }

    #[test]
    fn candidate_is_rejected_when_its_scope_changes_after_listing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = make_project(tmp.path(), "app", ".ai-memory.toml");
        std::fs::write(
            path.join(".ai-memory.toml"),
            b"workspace = \"one\"\nproject = \"app\"\n",
        )
        .unwrap();
        let candidate = Candidate {
            workspace: "one".to_owned(),
            project: "app".to_owned(),
            path: path.clone(),
            source: CandidateSource::Scan,
            page_count: None,
            last_updated: None,
            activity_us: None,
        };
        std::fs::write(
            path.join(".ai-memory.toml"),
            b"workspace = \"two\"\nproject = \"app\"\n",
        )
        .unwrap();

        assert!(revalidate_candidate(&config_at(tmp.path()), &candidate).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn registered_path_rejects_a_checkout_replaced_by_a_symlink() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::TempDir::new().unwrap();
        let checkout = tmp.path().join("checkout");
        let other = tmp.path().join("other");
        std::fs::create_dir(&checkout).unwrap();
        std::fs::create_dir(&other).unwrap();
        let stored = checkout.canonicalize().unwrap();
        std::fs::remove_dir(&checkout).unwrap();
        symlink(&other, &checkout).unwrap();

        assert!(validate_registered_path(&stored).is_err());
    }

    #[test]
    fn scroll_and_truncation_stay_inside_the_viewport() {
        assert_eq!(scroll_offset(5, 24, 5, 0), 1);
        assert_eq!(scroll_offset(23, 24, 5, 22), 19);
        for width in 0..12 {
            assert!(
                truncate("a considerably longer label", width)
                    .chars()
                    .count()
                    <= width
            );
        }
    }

    #[test]
    fn harness_names_cover_every_parser_variant() {
        for harness in RunHarnessChoice::value_variants() {
            assert_ne!(harness_name(*harness), "unknown");
        }
    }
}
