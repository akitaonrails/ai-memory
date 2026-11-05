//! Extra host folders an `ai-memory run --yolo` jail may mount, and the
//! terminal picker that chooses them.
//!
//! ai-jail mounts the checkout itself; these are *additional* folders (a
//! sibling project the agent should study). Validation is pure path logic plus
//! a canonicalized second pass so a symlink cannot smuggle a credential store
//! into the sandbox. The picker is a state machine ([`PickerState`]) driven by
//! a thin crossterm loop, so every key binding is testable without a terminal.

use std::cmp::Reverse;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ai_memory_core::i18n::Localizer;
use anyhow::{Context as _, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Stylize as _;
use crossterm::{cursor, execute, terminal};

use super::show::terminal_text;

/// One extra folder to mount into the jail.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ExtraFolder {
    pub(crate) path: PathBuf,
    #[serde(default)]
    pub(crate) writable: bool,
}

impl ExtraFolder {
    /// The folder's name and mode, e.g. `ai-jail (ro)`.
    pub(crate) fn label(&self) -> String {
        let name = self.path.file_name().map_or_else(
            || self.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        format!("{name} ({})", if self.writable { "rw" } else { "ro" })
    }
}

/// Add `folder`, replacing an earlier entry for the same path.
pub(crate) fn upsert(folders: &mut Vec<ExtraFolder>, folder: ExtraFolder) {
    folders.retain(|existing| existing.path != folder.path);
    folders.push(folder);
}

/// Why a folder cannot be mounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FolderRefusal {
    Missing,
    NotADirectory,
    /// The protected path the folder is, sits inside, or contains.
    Sensitive(PathBuf),
    IsCheckout,
}

impl std::fmt::Display for FolderRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("the folder does not exist"),
            Self::NotADirectory => f.write_str("it is not a folder"),
            Self::Sensitive(path) => write!(f, "it would expose {}", path.display()),
            Self::IsCheckout => f.write_str("the checkout is already mounted"),
        }
    }
}

impl FolderRefusal {
    /// The same sentences as [`Display`](std::fmt::Display), localized. Kept as
    /// literals so the catalog test sees every one of them.
    fn localized(&self, i18n: &Localizer, home: Option<&Path>) -> String {
        match self {
            Self::Missing => i18n.diagnostic("the folder does not exist"),
            Self::NotADirectory => i18n.diagnostic("it is not a folder"),
            Self::Sensitive(path) => i18n.diagnostic(&format!(
                "it would expose {}",
                terminal_text(&abbreviate(path, home))
            )),
            Self::IsCheckout => i18n.diagnostic("the checkout is already mounted"),
        }
    }
}

/// Facts injected so tests never touch the real home.
pub(crate) struct FolderRules<'a> {
    pub(crate) home: Option<&'a Path>,
    pub(crate) data_dir: &'a Path,
    pub(crate) checkout_root: &'a Path,
}

/// Credential and agent-state stores under the home directory. Mounting one,
/// a folder inside one, or a folder containing one would hand the jailed
/// agent the very secrets the jail exists to keep from it.
const SENSITIVE_HOME_DIRS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".kube",
    ".config/gh",
    ".config/gcloud",
    ".docker",
    ".local/share/ai-memory",
];

/// The protected paths [`validate_extra_folder`] compares against, in both
/// lexical and resolved form. Resolving them is the costly part, so the picker
/// builds this once per session instead of once per listed folder.
pub(crate) struct ProtectedPaths {
    home: Option<PathBuf>,
    checkout: PathBuf,
    protected: Vec<PathBuf>,
    resolved_home: Option<PathBuf>,
    resolved_checkout: PathBuf,
    resolved_protected: Vec<PathBuf>,
}

fn canonical(path: &Path) -> PathBuf {
    // A path that does not exist cannot be reached through a link and keeps
    // its lexical form.
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

impl ProtectedPaths {
    pub(crate) fn new(rules: &FolderRules<'_>, cwd: &Path) -> Self {
        let home = rules.home.map(|home| absolutize(home, cwd, None));
        let checkout = absolutize(rules.checkout_root, cwd, None);
        let mut protected = home
            .iter()
            .flat_map(|home| SENSITIVE_HOME_DIRS.iter().map(|dir| home.join(dir)))
            .collect::<Vec<_>>();
        protected.push(absolutize(rules.data_dir, cwd, None));
        Self {
            resolved_home: home.as_deref().map(canonical),
            resolved_checkout: canonical(&checkout),
            resolved_protected: protected.iter().map(|path| canonical(path)).collect(),
            home,
            checkout,
            protected,
        }
    }

    /// Refuse `path` (already absolute and lexical) when the jail must not
    /// mount it. A symlink (the folder itself or an ancestor) can point
    /// anywhere, so the resolved form must pass the same rules.
    fn check(&self, path: &Path) -> Result<(), FolderRefusal> {
        let metadata = std::fs::metadata(path).map_err(|_| FolderRefusal::Missing)?;
        if !metadata.is_dir() {
            return Err(FolderRefusal::NotADirectory);
        }
        refuse(path, self.home.as_deref(), &self.checkout, &self.protected)?;
        refuse(
            &canonical(path),
            self.resolved_home.as_deref(),
            &self.resolved_checkout,
            &self.resolved_protected,
        )
        .map_err(|refusal| match refusal {
            // Name the protected path the user knows, not its resolved target.
            FolderRefusal::Sensitive(hit) => FolderRefusal::Sensitive(
                self.resolved_protected
                    .iter()
                    .position(|candidate| *candidate == hit)
                    .map_or(hit, |index| self.protected[index].clone()),
            ),
            other => other,
        })
    }
}

/// Absolutize `raw` against `cwd` lexically and refuse folders the jail must
/// not mount. Returns the lexical path; the canonical form is only checked.
pub(crate) fn validate_extra_folder(
    raw: &Path,
    cwd: &Path,
    rules: &FolderRules<'_>,
) -> Result<PathBuf, FolderRefusal> {
    let path = absolutize(raw, cwd, rules.home);
    ProtectedPaths::new(rules, cwd).check(&path)?;
    Ok(path)
}

fn refuse(
    path: &Path,
    home: Option<&Path>,
    checkout: &Path,
    protected: &[PathBuf],
) -> Result<(), FolderRefusal> {
    if path.starts_with(checkout) {
        return Err(FolderRefusal::IsCheckout);
    }
    if path.parent().is_none() {
        return Err(FolderRefusal::Sensitive(path.to_owned()));
    }
    if home == Some(path) {
        return Err(FolderRefusal::Sensitive(path.to_owned()));
    }
    match protected
        .iter()
        .find(|guarded| path.starts_with(guarded) || guarded.starts_with(path))
    {
        Some(guarded) => Err(FolderRefusal::Sensitive(guarded.clone())),
        None => Ok(()),
    }
}

/// Lexical absolutization: `~` expansion, `cwd` join, `.`/`..` folding. Never
/// canonicalizes, so the returned path matches what the user typed (see the
/// macOS `/private/var` and Windows `\\?\` divergence canonicalize causes).
fn absolutize(raw: &Path, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let expanded = match (raw.strip_prefix("~"), home) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => raw.to_owned(),
    };
    ai_memory_workstream::clean_path(&cwd.join(expanded))
}

fn abbreviate(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// One subdirectory as the picker lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirEntryInfo {
    pub(crate) name: String,
    pub(crate) is_git: bool,
    pub(crate) modified: Option<SystemTime>,
}

/// The filesystem as the picker sees it, so tests can supply a fake tree.
pub(crate) trait DirSource {
    /// Immediate subdirectories of `dir`, unordered.
    fn list(&self, dir: &Path) -> Vec<DirEntryInfo>;

    /// Whether `path` (absolute) may be mounted.
    fn check(&self, path: &Path, rules: &FolderRules<'_>) -> Result<PathBuf, FolderRefusal> {
        validate_extra_folder(path, Path::new("/"), rules)
    }
}

/// The real filesystem, with the protected paths resolved once.
struct RealDirs {
    protected: ProtectedPaths,
}

impl DirSource for RealDirs {
    fn check(&self, path: &Path, rules: &FolderRules<'_>) -> Result<PathBuf, FolderRefusal> {
        let path = absolutize(path, Path::new("/"), rules.home);
        self.protected.check(&path)?;
        Ok(path)
    }

    fn list(&self, dir: &Path) -> Vec<DirEntryInfo> {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                // Follows symlinks so a linked project still lists; validation
                // resolves the link before it can be selected.
                let metadata = std::fs::metadata(&path).ok()?;
                if !metadata.is_dir() {
                    return None;
                }
                // A non-UTF-8 name could not round-trip through the filter and
                // the selection; such folders stay reachable by typing a path.
                let name = entry.file_name().into_string().ok()?;
                Some(DirEntryInfo {
                    name,
                    is_git: path.join(".git").exists(),
                    modified: metadata.modified().ok(),
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
struct Entry {
    name: String,
    path: PathBuf,
    is_git: bool,
    modified: Option<SystemTime>,
    refusal: Option<FolderRefusal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Step {
    Continue,
    Done(Vec<ExtraFolder>),
    Cancel,
}

/// Rendering facts the terminal loop supplies.
struct View<'a> {
    width: usize,
    list_rows: usize,
    color: bool,
    home: Option<&'a Path>,
    now: SystemTime,
}

struct PickerState {
    dir: PathBuf,
    query: String,
    path_mode: bool,
    cursor: usize,
    offset: usize,
    /// The current directory's listing, sorted, refusals precomputed.
    entries: Vec<Entry>,
    /// Path mode: children of the typed parent matching the typed prefix.
    candidates: Vec<Entry>,
    selected: Vec<ExtraFolder>,
    notice: Option<FolderRefusal>,
}

impl PickerState {
    fn new(
        start: &Path,
        preselected: &[ExtraFolder],
        fs: &impl DirSource,
        rules: &FolderRules<'_>,
    ) -> Self {
        let mut state = Self {
            dir: absolutize(start, Path::new("/"), rules.home),
            query: String::new(),
            path_mode: false,
            cursor: 0,
            offset: 0,
            entries: Vec::new(),
            candidates: Vec::new(),
            selected: preselected.to_vec(),
            notice: None,
        };
        state.load(fs, rules);
        state
    }

    fn load(&mut self, fs: &impl DirSource, rules: &FolderRules<'_>) {
        self.entries = entries_of(&self.dir, fs, rules);
        self.cursor = 0;
        self.offset = 0;
    }

    /// Indices into the list currently shown, best match first.
    fn visible(&self) -> Vec<usize> {
        if self.path_mode {
            return (0..self.candidates.len()).collect();
        }
        let query = self.query.to_lowercase();
        let show_hidden = query.starts_with('.');
        let mut hits = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| show_hidden || !entry.name.starts_with('.'))
            .filter_map(|(index, entry)| Some((match_rank(&entry.name, &query)?, index)))
            .collect::<Vec<_>>();
        // Stable: within a rank the listing's git-first, newest-first order holds.
        hits.sort_by_key(|(rank, _)| *rank);
        hits.into_iter().map(|(_, index)| index).collect()
    }

    fn shown(&self) -> &[Entry] {
        if self.path_mode {
            &self.candidates
        } else {
            &self.entries
        }
    }

    fn highlighted(&self) -> Option<&Entry> {
        let index = *self.visible().get(self.cursor)?;
        self.shown().get(index)
    }

    fn selection(&self, path: &Path) -> Option<usize> {
        self.selected.iter().position(|folder| folder.path == path)
    }

    fn handle(&mut self, key: KeyEvent, fs: &impl DirSource, rules: &FolderRules<'_>) -> Step {
        if key.kind != KeyEventKind::Press {
            return Step::Continue;
        }
        self.notice = None;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let count = self.visible().len();
        match key.code {
            KeyCode::Char('c') if control => return Step::Cancel,
            KeyCode::Char('w') if control => self.toggle_writable(),
            KeyCode::Up if count > 0 => {
                self.cursor = self.cursor.checked_sub(1).unwrap_or(count - 1);
            }
            KeyCode::Down if count > 0 => self.cursor = (self.cursor + 1) % count,
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = count.saturating_sub(1),
            KeyCode::Enter if self.path_mode => self.select_typed_path(fs, rules),
            KeyCode::Enter => return Step::Done(self.selected.clone()),
            KeyCode::Esc if !self.query.is_empty() => self.set_query(String::new(), fs, rules),
            KeyCode::Esc => return Step::Cancel,
            KeyCode::Tab if self.path_mode => self.complete(fs, rules),
            KeyCode::Right if self.path_mode => self.open_candidate(fs, rules),
            KeyCode::Tab | KeyCode::Right => self.descend(fs, rules),
            KeyCode::BackTab | KeyCode::Left if !self.path_mode => self.ascend(fs, rules),
            KeyCode::Backspace if self.query.is_empty() => self.ascend(fs, rules),
            KeyCode::Backspace => {
                let mut query = self.query.clone();
                query.pop();
                self.set_query(query, fs, rules);
            }
            KeyCode::Char(' ') if !self.path_mode => self.toggle_selected(),
            KeyCode::Char(character)
                if !character.is_control()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                let mut query = self.query.clone();
                query.push(character);
                self.set_query(query, fs, rules);
            }
            _ => {}
        }
        Step::Continue
    }

    fn set_query(&mut self, query: String, fs: &impl DirSource, rules: &FolderRules<'_>) {
        // `/` or `~` typed into an empty filter switches to path entry; any
        // other first character is a name filter. Emptying the query leaves
        // path mode again.
        self.path_mode = if self.query.is_empty() {
            query.starts_with(['/', '~'])
        } else {
            self.path_mode && !query.is_empty()
        };
        self.query = query;
        self.cursor = 0;
        self.offset = 0;
        self.candidates = if self.path_mode {
            self.path_candidates(fs, rules)
        } else {
            Vec::new()
        };
    }

    /// The typed parent directory and the partial last component.
    fn split_query(&self, home: Option<&Path>) -> (String, PathBuf, String) {
        let (head, partial) = match self.query.rfind('/') {
            Some(index) => (&self.query[..=index], &self.query[index + 1..]),
            // A bare `~`: its own children are the useful completion.
            None => ("", self.query.as_str()),
        };
        let parent = if head.is_empty() {
            absolutize(Path::new("~"), &self.dir, home)
        } else {
            absolutize(Path::new(head), &self.dir, home)
        };
        (head.to_owned(), parent, partial.to_owned())
    }

    fn path_candidates(&self, fs: &impl DirSource, rules: &FolderRules<'_>) -> Vec<Entry> {
        let (head, parent, partial) = self.split_query(rules.home);
        if head.is_empty() {
            return Vec::new();
        }
        entries_of(&parent, fs, rules)
            .into_iter()
            .filter(|entry| {
                entry.name.starts_with(&partial)
                    && (partial.starts_with('.') || !entry.name.starts_with('.'))
            })
            .collect()
    }

    /// Shell-style Tab: extend to the longest common prefix of the matching
    /// children, and close a unique match with `/` so the next Tab lists it.
    fn complete(&mut self, fs: &impl DirSource, rules: &FolderRules<'_>) {
        if self.query == "~" {
            self.set_query("~/".to_owned(), fs, rules);
            return;
        }
        let (head, _, partial) = self.split_query(rules.home);
        let names = self
            .candidates
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>();
        let Some(first) = names.first() else {
            return;
        };
        let common = names.iter().fold(first.to_string(), |common, name| {
            common
                .chars()
                .zip(name.chars())
                .take_while(|(left, right)| left == right)
                .map(|(character, _)| character)
                .collect()
        });
        let mut query = format!("{head}{}", common.max(partial));
        if names.len() == 1 {
            query.push('/');
        }
        self.set_query(query, fs, rules);
    }

    fn open_candidate(&mut self, fs: &impl DirSource, rules: &FolderRules<'_>) {
        if let Some(entry) = self.highlighted() {
            let (head, _, _) = self.split_query(rules.home);
            let query = format!("{head}{}/", entry.name);
            self.set_query(query, fs, rules);
        }
    }

    fn select_typed_path(&mut self, fs: &impl DirSource, rules: &FolderRules<'_>) {
        let typed = absolutize(Path::new(&self.query), &self.dir, rules.home);
        let mut outcome = fs.check(&typed, rules);
        // `~/Proj/ai-j` + Enter means the highlighted completion.
        if matches!(
            outcome,
            Err(FolderRefusal::Missing | FolderRefusal::NotADirectory)
        ) && let Some(entry) = self.highlighted()
        {
            outcome = fs.check(&entry.path, rules);
        }
        match outcome {
            Ok(path) => {
                if self.selection(&path).is_none() {
                    self.selected.push(ExtraFolder {
                        path,
                        writable: false,
                    });
                }
                self.set_query(String::new(), fs, rules);
            }
            Err(refusal) => self.notice = Some(refusal),
        }
    }

    fn toggle_selected(&mut self) {
        let Some(entry) = self.highlighted().cloned() else {
            return;
        };
        if let Some(index) = self.selection(&entry.path) {
            self.selected.remove(index);
        } else if let Some(refusal) = entry.refusal {
            self.notice = Some(refusal);
        } else {
            self.selected.push(ExtraFolder {
                path: entry.path,
                writable: false,
            });
        }
    }

    fn toggle_writable(&mut self) {
        let Some(path) = self.highlighted().map(|entry| entry.path.clone()) else {
            return;
        };
        if let Some(index) = self.selection(&path) {
            self.selected[index].writable = !self.selected[index].writable;
        }
    }

    fn descend(&mut self, fs: &impl DirSource, rules: &FolderRules<'_>) {
        // Browsing is always allowed, even through a refused folder such as
        // home itself: only mounting is guarded.
        if let Some(path) = self.highlighted().map(|entry| entry.path.clone()) {
            self.dir = path;
            self.query.clear();
            self.path_mode = false;
            self.load(fs, rules);
        }
    }

    fn ascend(&mut self, fs: &impl DirSource, rules: &FolderRules<'_>) {
        let Some(parent) = self.dir.parent().map(Path::to_owned) else {
            return;
        };
        let came_from = self.dir.clone();
        self.dir = parent;
        self.query.clear();
        self.path_mode = false;
        self.load(fs, rules);
        // Land on the folder just left, so Left then Right is a no-op.
        if let Some(position) = self
            .visible()
            .iter()
            .position(|index| self.entries[*index].path == came_from)
        {
            self.cursor = position;
        }
    }

    fn scroll(&mut self, rows: usize) {
        let rows = rows.max(1);
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + rows {
            self.offset = self.cursor + 1 - rows;
        }
    }

    fn render(&self, i18n: &Localizer, view: &View<'_>) -> Vec<String> {
        let mut lines = Vec::new();
        let mut header = Line::default();
        header.push(
            &format!(
                "📁 {}",
                i18n.diagnostic(&format!(
                    "Extra folders for the jail — {}",
                    terminal_text(&abbreviate(&self.dir, view.home))
                ))
            ),
            Paint::Bold,
        );
        lines.push(header.finish(view));

        let mut filter = Line::default();
        filter.push("> ", Paint::Cyan);
        filter.push(&terminal_text(&self.query), Paint::Bold);
        filter.push("▏", Paint::Cyan);
        lines.push(filter.finish(view));
        if let Some(notice) = &self.notice {
            let mut line = Line::default();
            line.push(
                &format!("  ✖ {}", notice.localized(i18n, view.home)),
                Paint::Red,
            );
            lines.push(line.finish(view));
        }

        let visible = self.visible();
        if visible.is_empty() {
            let mut line = Line::default();
            line.push(
                &format!("  {}", i18n.diagnostic("no matching folders")),
                Paint::Dim,
            );
            lines.push(line.finish(view));
        }
        let shown = self.shown();
        for (row, index) in visible
            .iter()
            .enumerate()
            .skip(self.offset)
            .take(view.list_rows.max(1))
        {
            lines.push(self.render_entry(&shown[*index], row == self.cursor, i18n, view));
        }

        let mut summary = Line::default();
        if self.selected.is_empty() {
            summary.push(&i18n.diagnostic("No extra folders selected"), Paint::Dim);
        } else {
            let names = self
                .selected
                .iter()
                .map(|folder| terminal_text(&folder.label()))
                .collect::<Vec<_>>()
                .join(", ");
            summary.push(&i18n.diagnostic(&format!("Selected: {names}")), Paint::Dim);
        }
        lines.push(summary.finish(view));
        let mut hint = Line::default();
        hint.push(
            &if self.path_mode {
                i18n.diagnostic("Tab complete · → open · Enter select · Esc back")
            } else {
                i18n.diagnostic(
                    "↑↓ move · Space select · Ctrl+W ro/rw · Tab open · ← up · Enter done · Esc cancel",
                )
            },
            Paint::Dim,
        );
        lines.push(hint.finish(view));
        lines
    }

    fn render_entry(
        &self,
        entry: &Entry,
        highlighted: bool,
        i18n: &Localizer,
        view: &View<'_>,
    ) -> String {
        let selected = self
            .selection(&entry.path)
            .map(|index| self.selected[index].writable);
        let mut line = Line::default();
        line.push(if highlighted { "❯ " } else { "  " }, Paint::Cyan);
        match selected {
            Some(_) => line.push("● ", Paint::Green),
            None => line.push("○ ", Paint::Dim),
        }
        let name = terminal_text(&entry.name);
        let name_paint = if entry.refusal.is_some() && selected.is_none() {
            Paint::Dim
        } else if highlighted {
            Paint::Bold
        } else {
            Paint::Plain
        };
        line.push(&name, name_paint);
        match selected {
            Some(true) => line.push(" [rw]", Paint::Yellow),
            Some(false) => line.push(" [ro]", Paint::Green),
            None => {}
        }
        let mut detail = Vec::new();
        if entry.is_git {
            detail.push("git".to_owned());
        }
        if let Some(age) = entry
            .modified
            .and_then(|modified| view.now.duration_since(modified).ok())
        {
            detail.push(i18n.age(i64::try_from(age.as_secs()).unwrap_or(i64::MAX)));
        }
        if !detail.is_empty() {
            line.push(&format!("  {}", detail.join(" · ")), Paint::Dim);
        }
        if let (Some(refusal), None) = (&entry.refusal, selected) {
            line.push(
                &format!("  ✖ {}", refusal.localized(i18n, view.home)),
                Paint::Red,
            );
        }
        line.finish(view)
    }
}

fn entries_of(dir: &Path, fs: &impl DirSource, rules: &FolderRules<'_>) -> Vec<Entry> {
    let mut entries = fs
        .list(dir)
        .into_iter()
        .map(|info| {
            let path = dir.join(&info.name);
            let refusal = fs.check(&path, rules).err();
            Entry {
                name: info.name,
                path,
                is_git: info.is_git,
                modified: info.modified,
                refusal,
            }
        })
        .collect::<Vec<_>>();
    // Projects first, then what was touched most recently: the sibling the
    // user wants is almost always a repo they just worked in.
    entries.sort_by(|left, right| {
        (Reverse(left.is_git), Reverse(left.modified), &left.name).cmp(&(
            Reverse(right.is_git),
            Reverse(right.modified),
            &right.name,
        ))
    });
    entries
}

/// Case-insensitive fuzzy match: 0 = prefix, 1 = substring, 2 = subsequence.
fn match_rank(name: &str, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let name = name.to_lowercase();
    if name.starts_with(query) {
        return Some(0);
    }
    if name.contains(query) {
        return Some(1);
    }
    let mut rest = name.chars();
    query
        .chars()
        .all(|wanted| rest.any(|character| character == wanted))
        .then_some(2)
}

#[derive(Debug, Clone, Copy)]
enum Paint {
    Plain,
    Bold,
    Dim,
    Red,
    Green,
    Yellow,
    Cyan,
}

/// A styled line truncated by display columns *before* styling, so escape
/// codes never count against the width and a line never wraps (wrapping
/// would break the cursor-up redraw).
#[derive(Default)]
struct Line {
    segments: Vec<(String, Paint)>,
}

impl Line {
    fn push(&mut self, text: &str, paint: Paint) {
        self.segments.push((text.to_owned(), paint));
    }

    fn finish(self, view: &View<'_>) -> String {
        let mut budget = view.width.saturating_sub(1).max(1);
        let mut out = String::new();
        for (text, paint) in self.segments {
            let mut kept = String::new();
            for character in text.chars() {
                let width = char_columns(character);
                if width > budget {
                    budget = 0;
                    break;
                }
                budget -= width;
                kept.push(character);
            }
            out.push_str(&paint_text(&kept, paint, view.color));
            if budget == 0 {
                break;
            }
        }
        out
    }
}

/// Close enough for the glyphs this picker prints: emoji are double width.
fn char_columns(character: char) -> usize {
    if u32::from(character) >= 0x1F300 {
        2
    } else {
        1
    }
}

fn paint_text(text: &str, paint: Paint, color: bool) -> String {
    if !color || text.is_empty() {
        return text.to_owned();
    }
    match paint {
        Paint::Plain => text.to_owned(),
        Paint::Bold => text.bold().to_string(),
        Paint::Dim => text.dark_grey().to_string(),
        Paint::Red => text.red().to_string(),
        Paint::Green => text.green().to_string(),
        Paint::Yellow => text.yellow().to_string(),
        Paint::Cyan => text.cyan().to_string(),
    }
}

/// Restores the terminal on every exit, including `?` and panics.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stderr(), cursor::Show);
    }
}

/// Interactive picker on the real terminal (crossterm raw mode, stderr),
/// starting in `start` (the checkout's parent). `preselected` are shown
/// selected. Returns `Ok(Some(list))` on Enter, `Ok(None)` on Esc.
pub(crate) fn pick_folders(
    i18n: &Localizer,
    start: &Path,
    preselected: &[ExtraFolder],
    rules: &FolderRules<'_>,
    color: bool,
) -> Result<Option<Vec<ExtraFolder>>> {
    let fs = RealDirs {
        protected: ProtectedPaths::new(rules, Path::new("/")),
    };
    let mut state = PickerState::new(start, preselected, &fs, rules);
    terminal::enable_raw_mode().context("entering raw mode")?;
    let _guard = TerminalGuard;
    let mut out = std::io::stderr();
    execute!(out, cursor::Hide)?;
    while event::poll(std::time::Duration::from_millis(0))? {
        let _ = event::read()?;
    }
    let mut drawn = 0usize;
    loop {
        let (columns, rows) = terminal::size().unwrap_or((80, 24));
        let view = View {
            width: usize::from(columns.max(20)),
            // Header, filter, notice and two footer lines stay on screen.
            list_rows: usize::from(rows).saturating_sub(6).clamp(3, 15),
            color,
            home: rules.home,
            now: SystemTime::now(),
        };
        state.scroll(view.list_rows);
        drawn = draw(&mut out, &state.render(i18n, &view), drawn)?;
        let Event::Key(key) = event::read()? else {
            continue;
        };
        match state.handle(key, &fs, rules) {
            Step::Continue => {}
            Step::Done(selected) => {
                draw(&mut out, &[], drawn)?;
                return Ok(Some(selected));
            }
            Step::Cancel => {
                draw(&mut out, &[], drawn)?;
                return Ok(None);
            }
        }
    }
}

fn draw(out: &mut std::io::Stderr, lines: &[String], previous: usize) -> Result<usize> {
    if let Ok(up) = u16::try_from(previous)
        && up > 0
    {
        execute!(out, cursor::MoveUp(up))?;
    }
    write!(out, "\r")?;
    execute!(out, terminal::Clear(terminal::ClearType::FromCursorDown))?;
    for line in lines {
        write!(out, "{line}\r\n")?;
    }
    out.flush()?;
    Ok(lines.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    struct Fixture {
        _root: tempfile::TempDir,
        home: PathBuf,
        data_dir: PathBuf,
        checkout: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::TempDir::new().unwrap();
            let home = root.path().join("home");
            let data_dir = root.path().join("data/ai-memory");
            let checkout = home.join("Projects/ai-memory");
            for dir in [
                home.join(".ssh/keys"),
                home.join(".config/gh"),
                home.join(".config/nvim"),
                home.join("Projects/ai-jail"),
                checkout.join("crates"),
                data_dir.clone(),
            ] {
                std::fs::create_dir_all(dir).unwrap();
            }
            std::fs::write(home.join("Projects/notes.txt"), "x").unwrap();
            Self {
                _root: root,
                home,
                data_dir,
                checkout,
            }
        }

        fn rules(&self) -> FolderRules<'_> {
            FolderRules {
                home: Some(&self.home),
                data_dir: &self.data_dir,
                checkout_root: &self.checkout,
            }
        }

        fn check(&self, raw: &str) -> Result<PathBuf, FolderRefusal> {
            validate_extra_folder(Path::new(raw), &self.checkout, &self.rules())
        }
    }

    #[test]
    fn jail_folders_validate_accepts_a_sibling_and_returns_the_lexical_path() {
        let fixture = Fixture::new();
        let sibling = fixture.home.join("Projects/ai-jail");
        assert_eq!(fixture.check("../ai-jail"), Ok(sibling.clone()));
        assert_eq!(fixture.check("~/Projects/ai-jail"), Ok(sibling.clone()));
        assert_eq!(
            fixture.check("~/Projects/./ai-memory/../ai-jail/"),
            Ok(sibling.clone())
        );
        assert_eq!(fixture.check(sibling.to_str().unwrap()), Ok(sibling));
        assert_eq!(
            fixture.check("~/.config/nvim"),
            Ok(fixture.home.join(".config/nvim"))
        );
    }

    #[test]
    fn jail_folders_validate_refuses_missing_files_and_the_checkout() {
        let fixture = Fixture::new();
        assert_eq!(fixture.check("../nope"), Err(FolderRefusal::Missing));
        assert_eq!(
            fixture.check("../notes.txt"),
            Err(FolderRefusal::NotADirectory)
        );
        assert_eq!(fixture.check("."), Err(FolderRefusal::IsCheckout));
        assert_eq!(fixture.check("crates"), Err(FolderRefusal::IsCheckout));
    }

    #[test]
    fn jail_folders_validate_refuses_root_home_and_credential_stores() {
        let fixture = Fixture::new();
        let ssh = fixture.home.join(".ssh");
        assert_eq!(
            fixture.check("/"),
            Err(FolderRefusal::Sensitive(PathBuf::from("/")))
        );
        assert_eq!(
            fixture.check("~"),
            Err(FolderRefusal::Sensitive(fixture.home.clone()))
        );
        assert_eq!(
            fixture.check("~/.ssh"),
            Err(FolderRefusal::Sensitive(ssh.clone()))
        );
        assert_eq!(
            fixture.check("~/.ssh/keys"),
            Err(FolderRefusal::Sensitive(ssh))
        );
        // Containing a credential store is as bad as being one.
        assert_eq!(
            fixture.check("~/.config"),
            Err(FolderRefusal::Sensitive(fixture.home.join(".config/gh")))
        );
        assert_eq!(
            fixture.check(fixture.home.parent().unwrap().to_str().unwrap()),
            Err(FolderRefusal::Sensitive(fixture.home.join(".ssh")))
        );
    }

    #[test]
    fn jail_folders_validate_refuses_the_data_dir_and_its_parent() {
        let fixture = Fixture::new();
        let data = fixture.data_dir.to_str().unwrap().to_owned();
        let parent = fixture
            .data_dir
            .parent()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let refused = Err(FolderRefusal::Sensitive(fixture.data_dir.clone()));
        assert_eq!(fixture.check(&data), refused);
        assert_eq!(fixture.check(&parent), refused);
    }

    #[cfg(unix)]
    #[test]
    fn jail_folders_validate_refuses_a_symlink_into_a_credential_store() {
        let fixture = Fixture::new();
        let link = fixture.home.join("Projects/innocent");
        std::os::unix::fs::symlink(fixture.home.join(".ssh/keys"), &link).unwrap();
        assert_eq!(
            fixture.check("../innocent"),
            Err(FolderRefusal::Sensitive(fixture.home.join(".ssh")))
        );
        // A link to the checkout is the checkout.
        let mirror = fixture.home.join("Projects/mirror");
        std::os::unix::fs::symlink(&fixture.checkout, &mirror).unwrap();
        assert_eq!(fixture.check("../mirror"), Err(FolderRefusal::IsCheckout));
        // A legitimate link still returns its own lexical path.
        let alias = fixture.home.join("Projects/jail-alias");
        std::os::unix::fs::symlink(fixture.home.join("Projects/ai-jail"), &alias).unwrap();
        assert_eq!(fixture.check("../jail-alias"), Ok(alias));
    }

    #[test]
    fn jail_folders_refusals_read_as_sentences() {
        let i18n = Localizer::default();
        let home = Path::new("/home/u");
        let ssh = FolderRefusal::Sensitive(home.join(".ssh"));
        assert_eq!(ssh.to_string(), "it would expose /home/u/.ssh");
        assert_eq!(ssh.localized(&i18n, Some(home)), "it would expose ~/.ssh");
        assert_eq!(
            FolderRefusal::Missing.localized(&i18n, None),
            FolderRefusal::Missing.to_string()
        );
    }

    /// An in-memory tree: `dirs` maps a directory to its children; `refused`
    /// overrides validation for chosen paths.
    #[derive(Default)]
    struct FakeDirs {
        dirs: HashMap<PathBuf, Vec<DirEntryInfo>>,
        refused: HashMap<PathBuf, FolderRefusal>,
    }

    impl FakeDirs {
        fn dir(mut self, dir: &str, children: &[(&str, bool, u64)]) -> Self {
            let children = children
                .iter()
                .map(|(name, is_git, age)| DirEntryInfo {
                    name: (*name).to_owned(),
                    is_git: *is_git,
                    modified: Some(now() - Duration::from_secs(*age)),
                })
                .collect();
            self.dirs.insert(PathBuf::from(dir), children);
            self
        }

        fn refuse(mut self, path: &str, refusal: FolderRefusal) -> Self {
            self.refused.insert(PathBuf::from(path), refusal);
            self
        }

        fn exists(&self, path: &Path) -> bool {
            self.dirs.contains_key(path)
                || path
                    .parent()
                    .and_then(|parent| self.dirs.get(parent))
                    .is_some_and(|children| {
                        children.iter().any(|child| {
                            path.file_name()
                                .is_some_and(|name| name == child.name.as_str())
                        })
                    })
        }
    }

    impl DirSource for FakeDirs {
        fn list(&self, dir: &Path) -> Vec<DirEntryInfo> {
            self.dirs.get(dir).cloned().unwrap_or_default()
        }

        fn check(&self, path: &Path, _: &FolderRules<'_>) -> Result<PathBuf, FolderRefusal> {
            if let Some(refusal) = self.refused.get(path) {
                return Err(refusal.clone());
            }
            if self.exists(path) {
                Ok(path.to_owned())
            } else {
                Err(FolderRefusal::Missing)
            }
        }
    }

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000)
    }

    const HOME: &str = "/home/u";

    fn rules() -> FolderRules<'static> {
        FolderRules {
            home: Some(Path::new(HOME)),
            data_dir: Path::new("/data"),
            checkout_root: Path::new("/home/u/Projects/ai-memory"),
        }
    }

    fn tree() -> FakeDirs {
        FakeDirs::default()
            .dir(
                "/home/u/Projects",
                &[
                    ("scratch", false, 10),
                    ("homebrew-tap", true, 7_200),
                    ("ai-jail", true, 60),
                    ("ai-memory", true, 5),
                    (".hidden", false, 1),
                ],
            )
            .dir("/home/u/Projects/ai-jail", &[("src", false, 30)])
            .dir("/home/u", &[("Projects", false, 5), (".ssh", false, 1)])
            .refuse("/home/u/Projects/ai-memory", FolderRefusal::IsCheckout)
            .refuse(
                "/home/u/.ssh",
                FolderRefusal::Sensitive(PathBuf::from("/home/u/.ssh")),
            )
    }

    fn picker(fs: &FakeDirs, preselected: &[ExtraFolder]) -> PickerState {
        PickerState::new(Path::new("/home/u/Projects"), preselected, fs, &rules())
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn press(state: &mut PickerState, fs: &FakeDirs, codes: &[KeyCode]) -> Step {
        let mut step = Step::Continue;
        for code in codes {
            step = state.handle(key(*code), fs, &rules());
        }
        step
    }

    fn typed(state: &mut PickerState, fs: &FakeDirs, text: &str) {
        for character in text.chars() {
            state.handle(key(KeyCode::Char(character)), fs, &rules());
        }
    }

    fn names(state: &PickerState) -> Vec<String> {
        state
            .visible()
            .iter()
            .map(|index| state.shown()[*index].name.clone())
            .collect()
    }

    fn folder(path: &str, writable: bool) -> ExtraFolder {
        ExtraFolder {
            path: PathBuf::from(path),
            writable,
        }
    }

    fn rendered(state: &PickerState) -> String {
        let view = View {
            width: 200,
            list_rows: 10,
            color: false,
            home: Some(Path::new(HOME)),
            now: now(),
        };
        state.render(&Localizer::default(), &view).join("\n")
    }

    #[test]
    fn jail_folders_picker_lists_git_repos_first_then_newest_and_hides_dot_dirs() {
        let fs = tree();
        let state = picker(&fs, &[]);
        assert_eq!(
            names(&state),
            ["ai-memory", "ai-jail", "homebrew-tap", "scratch"]
        );
    }

    #[test]
    fn jail_folders_picker_filters_by_fuzzy_subsequence() {
        let fs = tree();
        let mut state = picker(&fs, &[]);
        typed(&mut state, &fs, "aij");
        assert_eq!(names(&state), ["ai-jail"]);
        // Typing `r` filters; it is not a shortcut.
        typed(&mut state, &fs, "r");
        assert!(names(&state).is_empty());
        press(&mut state, &fs, &[KeyCode::Backspace, KeyCode::Backspace]);
        assert_eq!(state.query, "ai");
        assert_eq!(names(&state)[0], "ai-memory");
        press(&mut state, &fs, &[KeyCode::Esc]);
        typed(&mut state, &fs, ".h");
        assert_eq!(names(&state), [".hidden"]);
    }

    #[test]
    fn jail_folders_picker_selects_read_only_and_ctrl_w_flips_to_rw() {
        let fs = tree();
        let mut state = picker(&fs, &[]);
        press(&mut state, &fs, &[KeyCode::Down, KeyCode::Char(' ')]);
        assert_eq!(state.selected, [folder("/home/u/Projects/ai-jail", false)]);
        state.handle(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &fs,
            &rules(),
        );
        assert_eq!(state.selected, [folder("/home/u/Projects/ai-jail", true)]);
        assert_eq!(state.query, "", "Ctrl+W must not type into the filter");
        press(&mut state, &fs, &[KeyCode::Char(' ')]);
        assert!(state.selected.is_empty());
    }

    #[test]
    fn jail_folders_picker_refuses_to_select_a_refused_entry() {
        let fs = tree();
        let mut state = picker(&fs, &[]);
        press(&mut state, &fs, &[KeyCode::Char(' ')]);
        assert!(state.selected.is_empty());
        assert_eq!(state.notice, Some(FolderRefusal::IsCheckout));
        let screen = rendered(&state);
        assert!(
            screen.contains("✖ the checkout is already mounted"),
            "{screen}"
        );
    }

    #[test]
    fn jail_folders_picker_navigation_keeps_selections() {
        let fs = tree();
        let mut state = picker(&fs, &[]);
        press(
            &mut state,
            &fs,
            &[KeyCode::Down, KeyCode::Char(' '), KeyCode::Tab],
        );
        assert_eq!(state.dir, Path::new("/home/u/Projects/ai-jail"));
        assert_eq!(names(&state), ["src"]);
        press(&mut state, &fs, &[KeyCode::Char(' '), KeyCode::Left]);
        assert_eq!(state.dir, Path::new("/home/u/Projects"));
        assert_eq!(
            state.highlighted().map(|entry| entry.name.as_str()),
            Some("ai-jail"),
            "going up lands on the folder just left"
        );
        press(&mut state, &fs, &[KeyCode::Right, KeyCode::BackTab]);
        assert_eq!(state.dir, Path::new("/home/u/Projects"));
        // Backspace on an empty filter goes up too, and descending through a
        // refused folder (home) is allowed.
        press(&mut state, &fs, &[KeyCode::Backspace]);
        assert_eq!(state.dir, Path::new(HOME));
        assert_eq!(
            state.selected,
            [
                folder("/home/u/Projects/ai-jail", false),
                folder("/home/u/Projects/ai-jail/src", false)
            ]
        );
    }

    #[test]
    fn jail_folders_picker_esc_clears_the_query_then_cancels_and_enter_returns() {
        let fs = tree();
        let mut state = picker(&fs, &[]);
        typed(&mut state, &fs, "tap");
        assert_eq!(press(&mut state, &fs, &[KeyCode::Esc]), Step::Continue);
        assert_eq!(state.query, "");
        assert_eq!(press(&mut state, &fs, &[KeyCode::Esc]), Step::Cancel);

        let mut state = picker(&fs, &[]);
        typed(&mut state, &fs, "tap");
        let step = press(&mut state, &fs, &[KeyCode::Char(' '), KeyCode::Enter]);
        assert_eq!(
            step,
            Step::Done(vec![folder("/home/u/Projects/homebrew-tap", false)])
        );
    }

    #[test]
    fn jail_folders_picker_shows_preselected_entries_selected() {
        let fs = tree();
        let state = picker(&fs, &[folder("/home/u/Projects/homebrew-tap", true)]);
        let screen = rendered(&state);
        assert!(screen.contains("● homebrew-tap [rw]"), "{screen}");
        assert!(screen.contains("○ ai-jail"), "{screen}");
        assert!(screen.contains("Selected: homebrew-tap (rw)"), "{screen}");
    }

    #[test]
    fn jail_folders_picker_path_mode_completes_and_selects() {
        let fs = tree();
        let mut state = picker(&fs, &[]);
        typed(&mut state, &fs, "~");
        assert!(state.path_mode);
        press(&mut state, &fs, &[KeyCode::Tab]);
        assert_eq!(state.query, "~/");
        typed(&mut state, &fs, "Pr");
        press(&mut state, &fs, &[KeyCode::Tab]);
        assert_eq!(state.query, "~/Projects/");
        typed(&mut state, &fs, "a");
        press(&mut state, &fs, &[KeyCode::Tab]);
        assert_eq!(state.query, "~/Projects/ai-", "longest common prefix");
        assert_eq!(names(&state), ["ai-memory", "ai-jail"]);
        typed(&mut state, &fs, "j");
        press(&mut state, &fs, &[KeyCode::Enter]);
        assert!(!state.path_mode);
        assert_eq!(state.query, "");
        assert_eq!(state.selected, [folder("/home/u/Projects/ai-jail", false)]);

        // An absolute path to a refused folder stays in path mode with a reason.
        typed(&mut state, &fs, "/home/u/.ssh");
        press(&mut state, &fs, &[KeyCode::Enter]);
        assert!(state.path_mode);
        assert!(matches!(state.notice, Some(FolderRefusal::Sensitive(_))));
        assert!(rendered(&state).contains("✖ it would expose ~/.ssh"));
        assert_eq!(state.selected.len(), 1);
        // Clearing the query leaves path mode.
        press(&mut state, &fs, &[KeyCode::Esc]);
        assert!(!state.path_mode);
    }

    #[test]
    fn jail_folders_render_shows_header_badges_and_hints_without_color() {
        let fs = tree();
        let mut state = picker(
            &fs,
            &[
                folder("/home/u/Projects/ai-jail", false),
                folder("/home/u/Projects/homebrew-tap", true),
            ],
        );
        let screen = rendered(&state);
        assert!(!screen.contains('\u{1b}'), "styling must be off: {screen}");
        let first = screen.lines().next().unwrap();
        assert_eq!(first, "📁 Extra folders for the jail — ~/Projects");
        assert!(
            screen.contains("● ai-jail [ro]  git · 1 minute ago"),
            "{screen}"
        );
        assert!(
            screen.contains("● homebrew-tap [rw]  git · 2 hours ago"),
            "{screen}"
        );
        assert!(
            screen.contains("✖ the checkout is already mounted"),
            "{screen}"
        );
        assert!(screen.contains("Selected: ai-jail (ro), homebrew-tap (rw)"));
        assert!(screen.contains("Ctrl+W ro/rw"));
        state.selected.clear();
        assert!(rendered(&state).contains("No extra folders selected"));
    }

    #[test]
    fn jail_folders_render_truncates_to_the_terminal_width() {
        let fs = tree();
        let state = picker(&fs, &[]);
        let view = View {
            width: 20,
            list_rows: 10,
            color: true,
            home: Some(Path::new(HOME)),
            now: now(),
        };
        for line in state.render(&Localizer::default(), &view) {
            let plain = strip_ansi(&line);
            let columns: usize = plain.chars().map(char_columns).sum();
            assert!(columns < 20, "{plain:?}");
        }
    }

    fn strip_ansi(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(character) = chars.next() {
            if character == '\u{1b}' {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(character);
            }
        }
        out
    }
}
