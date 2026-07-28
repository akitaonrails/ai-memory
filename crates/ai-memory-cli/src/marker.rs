//! `.ai-memory.toml` marker discovery and the scope it declares.
//!
//! One reader shared by both entry points that care about a repository's
//! declared identity:
//!
//! - the native hook path (`commands::hook_capture`), which forwards the
//!   marker's fields to the server as query params, and
//! - the thin-client CLI commands, which resolve `(workspace, project)`
//!   locally before calling `/admin/*` (see `commands::resolve_scope`).
//!
//! Before this module existed only the hook path read the marker, so a
//! repository could declare `workspace = "x"` and still have `run`,
//! `bootstrap`, `search` and friends silently resolve into `default` —
//! splitting one checkout across two scopes.
//!
//! Parsing is deliberately line-based (`parse_toml_key` / `parse_toml_flag`)
//! and mirrors `hooks/lib/_lib.sh`, so the native binary and the POSIX shell
//! hooks agree on what a marker means. `[capture]` is the one section parsed
//! strictly, with a real TOML parser, and stays in `hook_capture`.

use std::path::{Path, PathBuf};

use crate::commands::path_util::home_dir;
use crate::config::RuntimeEnv;

/// The scope fields a marker declares, plus where it was found.
///
/// `project` is what the marker pins directly; a marker that only sets
/// `project_strategy = "repo-root"` leaves it `None` and lets the caller
/// derive the name (see [`repo_root_project`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MarkerScope {
    /// Absolute path of the marker file the walk settled on.
    pub(crate) path: PathBuf,
    /// `workspace = "…"`.
    pub(crate) workspace: Option<String>,
    /// `project = "…"`.
    pub(crate) project: Option<String>,
    /// `project_strategy = "…"`, or the install-wide env default.
    pub(crate) project_strategy: Option<String>,
}

impl MarkerScope {
    /// Whether this marker declares anything that changes scope resolution.
    /// A marker that only carries `[capture]` rules does not.
    pub(crate) fn declares_scope(&self) -> bool {
        self.workspace.is_some() || self.project.is_some() || self.is_repo_root()
    }

    /// Whether the effective strategy asks for repo-root project naming.
    /// Accepts both spellings, matching the shell hooks.
    pub(crate) fn is_repo_root(&self) -> bool {
        matches!(
            self.project_strategy.as_deref(),
            Some("repo-root" | "repo_root")
        )
    }
}

/// Read the nearest marker's scope declaration, honouring
/// `AI_MEMORY_IGNORE_MARKER`.
///
/// Both env knobs arrive through [`RuntimeEnv`] rather than being read here:
/// `Config::load()` is the one config-read path, and routing them through it
/// is also what makes the caller's unit tests hermetic against a developer's
/// exported `AI_MEMORY_IGNORE_MARKER`.
///
/// Returns `None` when no marker is found, when the operator disabled marker
/// resolution, or when the marker declares nothing scope-related — callers
/// then keep their existing fallbacks untouched.
pub(crate) fn read_scope(cwd: &str, env: &RuntimeEnv) -> Option<MarkerScope> {
    if env.ignore_marker() {
        return None;
    }
    let path = find_marker_with_home(cwd, env.home_dir().map(Path::new))?;
    // One read, three keys: the marker is re-read per key nowhere else on a
    // hot path, but this one runs on every client command.
    let text = std::fs::read_to_string(&path).ok()?;
    let mut scope = MarkerScope {
        workspace: parse_key_in(&text, "workspace"),
        project: parse_key_in(&text, "project"),
        project_strategy: parse_key_in(&text, "project_strategy"),
        path,
    };
    if scope.project_strategy.is_none() {
        // The install-wide `--project-strategy` default, if one was baked into
        // the environment. Empty values are treated as unset.
        scope.project_strategy = env
            .project_strategy()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned);
    }
    scope.declares_scope().then_some(scope)
}

/// Derive a project name from the **main** repository root, so linked
/// worktrees and subdirectories collapse onto one project.
pub(crate) fn repo_root_project(cwd: &str) -> Option<String> {
    let root = ai_memory_consolidate::discover_main_repo_root(Path::new(cwd)).ok()?;
    root.file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Walk up from `cwd` toward `$HOME` looking for `.ai-memory.toml`.
/// Checkouts outside `$HOME` stop at their nearest `.git` root; a non-git
/// directory outside `$HOME` checks only `cwd`. When home is unavailable the
/// historical filesystem-root walk remains the fallback.
pub(crate) fn find_marker(cwd: &str) -> Option<PathBuf> {
    let home = home_dir();
    find_marker_with_home(cwd, home.as_deref())
}

fn find_marker_with_home(cwd: &str, home: Option<&Path>) -> Option<PathBuf> {
    let start = absolute_normalized(Path::new(cwd));
    let home = home.map(absolute_normalized);
    let boundary = match home.as_deref() {
        Some(home) if start.starts_with(home) => Some(home.to_path_buf()),
        Some(_) => Some(checkout_root(&start).unwrap_or_else(|| start.clone())),
        None => None,
    };

    let mut dir = start.as_path();
    loop {
        let candidate = dir.join(".ai-memory.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if boundary.as_deref() == Some(dir) {
            return None;
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent,
            _ => return None,
        }
    }
}

fn absolute_normalized(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        }
    })
}

fn checkout_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent,
            _ => return None,
        }
    }
}

/// Parse a root-level `key = "value"` line (no nesting, arrays, or
/// tables), mirroring `ai_memory_parse_toml_key`. Returns the first
/// match. Avoids pulling in a TOML parser dependency.
pub(crate) fn parse_toml_key(file: &Path, key: &str) -> Option<String> {
    parse_key_in(&std::fs::read_to_string(file).ok()?, key)
}

/// [`parse_toml_key`] over already-read marker text, so a caller that needs
/// several keys pays for one read.
fn parse_key_in(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(after_key) = trimmed.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = after_key.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// Parse a root-level `key = <value>` line, accepting a quoted string
/// (`key = "true"`) OR a bare token (`key = true` / `key = 1`), so a
/// `[recall] default_global = true` marker works whether or not the operator
/// quotes the value. Line-based like [`parse_toml_key`], so section headers
/// are ignored; strips an optional trailing `# comment`.
pub(crate) fn parse_toml_flag(file: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(file).ok()?;
    for line in text.lines() {
        let trimmed = line.trim_start();
        let Some(after_key) = trimmed.strip_prefix(key) else {
            continue;
        };
        let Some(rest) = after_key.trim_start().strip_prefix('=') else {
            continue;
        };
        let val = rest
            .split('#')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"');
        if !val.is_empty() {
            return Some(val.to_string());
        }
    }
    None
}

/// Shell-parity truthiness for marker flags and the ignore switch.
pub(crate) fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_marker(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join(".ai-memory.toml");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn find_marker_walks_up_from_cwd() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let marker = write_marker(&root, "workspace = \"acme\"\n");
        let nested = root.join("a").join("b");
        fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            find_marker_with_home(nested.to_str().unwrap(), Some(&root)),
            Some(marker),
            "a marker in any ancestor wins"
        );
    }

    #[test]
    fn find_marker_returns_none_without_one() {
        let tmp = TempDir::new().unwrap();
        assert_eq!(find_marker(tmp.path().to_str().unwrap()), None);
    }

    #[test]
    fn marker_walk_outside_home_stops_at_checkout_root() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let outer = tmp.path().join("outside");
        let repo = outer.join("repo");
        let nested = repo.join("src");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&home).unwrap();
        let outer_marker = write_marker(&outer, "workspace = \"wrong\"\n");
        let repo_marker = write_marker(&repo, "workspace = \"right\"\n");

        assert_eq!(
            find_marker_with_home(nested.to_str().unwrap(), Some(&home)),
            Some(repo_marker.canonicalize().unwrap())
        );
        fs::remove_file(repo.join(".ai-memory.toml")).unwrap();
        assert_eq!(
            find_marker_with_home(nested.to_str().unwrap(), Some(&home)),
            None,
            "a marker above the checkout boundary must not leak in"
        );
        assert!(outer_marker.exists());
    }

    #[test]
    fn marker_walk_outside_home_checks_only_cwd_without_a_checkout() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let outer = tmp.path().join("outside");
        let cwd = outer.join("plain");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        write_marker(&outer, "workspace = \"wrong\"\n");

        assert_eq!(
            find_marker_with_home(cwd.to_str().unwrap(), Some(&home)),
            None
        );
        let local = write_marker(&cwd, "workspace = \"right\"\n");
        assert_eq!(
            find_marker_with_home(cwd.to_str().unwrap(), Some(&home)),
            Some(local.canonicalize().unwrap())
        );
    }

    /// Happy-path TOML parser: extracts each declared root-level
    /// `key = "value"` pair. Mirrors the shell `ai_memory_parse_toml_key`.
    #[test]
    fn parse_toml_key_extracts_root_level_strings() {
        let tmp = TempDir::new().unwrap();
        let marker = write_marker(
            tmp.path(),
            r#"
workspace = "acme"
project = "infra"
project_strategy = "repo-root"
"#,
        );
        assert_eq!(
            parse_toml_key(&marker, "workspace").as_deref(),
            Some("acme")
        );
        assert_eq!(parse_toml_key(&marker, "project").as_deref(), Some("infra"));
        assert_eq!(
            parse_toml_key(&marker, "project_strategy").as_deref(),
            Some("repo-root")
        );
        assert_eq!(parse_toml_key(&marker, "absent"), None);
    }

    /// Shapes the naive parser deliberately doesn't handle (parity with
    /// the shell `_lib.sh` helper) — pin the contract so a future
    /// "robustify" refactor doesn't silently start matching them.
    #[test]
    fn parse_toml_key_skips_unsupported_shapes() {
        let tmp = TempDir::new().unwrap();
        let marker = write_marker(
            tmp.path(),
            r#"
# Single-quoted values are not honoured.
workspace = 'acme'
# Comments after the value are not stripped.
project = "infra" # this is fine
"#,
        );
        assert_eq!(parse_toml_key(&marker, "workspace"), None);
        // The trailing comment is appended to the value because the parser
        // looks for the first `"` — pin it so the contract is explicit.
        assert_eq!(parse_toml_key(&marker, "project").as_deref(), Some("infra"));
    }

    #[test]
    fn parse_toml_flag_accepts_bare_and_quoted_tokens() {
        let tmp = TempDir::new().unwrap();
        let marker = write_marker(
            tmp.path(),
            "[recall]\ndefault_global = true\nmax_chars = 4000 # budget\n",
        );

        assert_eq!(
            parse_toml_flag(&marker, "default_global").as_deref(),
            Some("true")
        );
        assert_eq!(
            parse_toml_flag(&marker, "max_chars").as_deref(),
            Some("4000"),
            "trailing comments are stripped"
        );
    }

    #[test]
    fn is_truthy_matches_shell_parity_tokens() {
        for value in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(is_truthy(value), "{value} should be truthy");
        }
        for value in ["0", "false", "", "maybe"] {
            assert!(!is_truthy(value), "{value} should not be truthy");
        }
    }

    #[test]
    fn scope_declares_repo_root_for_both_spellings() {
        let base = MarkerScope {
            path: PathBuf::from("/tmp/.ai-memory.toml"),
            workspace: None,
            project: None,
            project_strategy: None,
        };

        for spelling in ["repo-root", "repo_root"] {
            let scope = MarkerScope {
                project_strategy: Some(spelling.to_string()),
                ..base.clone()
            };
            assert!(scope.is_repo_root(), "{spelling} should select repo-root");
            assert!(
                scope.declares_scope(),
                "{spelling} alone still changes resolution"
            );
        }

        assert!(
            !base.declares_scope(),
            "a marker with only [capture] rules must not change scope"
        );
    }
}
