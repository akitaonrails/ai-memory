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
//! and mirrors `hooks/_lib.sh`, so the native binary and the POSIX shell
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
    let path = find_settings_marker_with_home(cwd, env.home_dir().map(Path::new))?;
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
    find_marker_matching(cwd, home, |_| true)
}

/// Like [`find_marker`], but skips a marker that declares nothing beyond a
/// `[capture]` section (scope/settings-*transparent*) and continues the walk
/// to the next ancestor. Resolves `workspace`/`project`/`project_strategy`
/// and the other root-level settings `hook_capture` forwards, so a nested
/// capture-only marker no longer resets them to their fallback (#668).
/// `[capture]`/`ignore_paths` itself keeps using [`find_marker`] — the
/// nearest marker, unchanged.
pub(crate) fn find_settings_marker(cwd: &str) -> Option<PathBuf> {
    let home = home_dir();
    find_settings_marker_with_home(cwd, home.as_deref())
}

fn find_settings_marker_with_home(cwd: &str, home: Option<&Path>) -> Option<PathBuf> {
    find_marker_matching(cwd, home, |path| {
        std::fs::read_to_string(path).is_ok_and(|text| declares_more_than_capture(&text))
    })
}

/// Shared walk-up-from-`cwd`-toward-`$HOME` used by [`find_marker_with_home`]
/// and [`find_settings_marker_with_home`]; `matches` decides whether a marker
/// file found along the way stops the walk (returned) or is skipped in favor
/// of the next ancestor. The HOME/checkout-root boundary is identical either
/// way — only which markers count as a stopping point differs.
fn find_marker_matching(
    cwd: &str,
    home: Option<&Path>,
    matches: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
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
        if candidate.is_file() && matches(&candidate) {
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

/// Whether a marker's raw text declares anything beyond a `[capture]`
/// section: any root-level scope key (`workspace`/`project`/
/// `project_strategy`), or any of the other settings
/// `hook_capture::marker_query_suffix_impl` forwards (`[recall]
/// default_global`, `[briefing]` keys, top-level `drop_subagent_captures`).
/// A marker with any of these is a resolution boundary; only a marker whose
/// only content is `[capture]` (e.g. `ignore_paths`) is transparent (#668).
///
/// Line-based like [`parse_key_in`] / [`parse_toml_flag`] — section headers
/// are not tracked, so a stray key is still detected wherever it appears in
/// the file. That is conservative on purpose: it can only turn a marker INTO
/// a boundary, never wrongly make one transparent.
fn declares_more_than_capture(text: &str) -> bool {
    const QUOTED_KEYS: [&str; 4] = [
        "workspace",
        "project",
        "project_strategy",
        "drop_subagent_captures",
    ];
    const FLAG_KEYS: [&str; 3] = ["default_global", "inject_on_session_start", "max_chars"];
    QUOTED_KEYS
        .iter()
        .any(|key| parse_key_in(text, key).is_some())
        || FLAG_KEYS
            .iter()
            .any(|key| parse_flag_in(text, key).is_some())
}

/// Make `path` absolute and resolve its `.`/`..` components, WITHOUT
/// touching the filesystem: no symlink resolution, no `\\?\` verbatim
/// prefix. `find_marker`'s callers (`hook_capture::capture_policy` ->
/// `CapturePolicy::compile`) compare the returned marker directory, as a
/// plain string prefix, against runtime candidate paths straight from the
/// hook payload (`cwd`/`tool_input.file_path`) — paths the hook host never
/// canonicalizes. A `fs::canonicalize`-based normalization here used to
/// resolve macOS's `/var` -> `/private/var` symlink and prepend Windows'
/// `\\?\` prefix, moving the marker path into a different namespace than
/// the candidate and making `[capture] ignore_paths` glob matching
/// silently miss on both platforms (#671). Lexical normalization keeps
/// `start`, `home`, the marker path and `checkout_root` in the caller's own
/// namespace on every platform, and still resolves `..` traversal purely
/// syntactically, preserving the boundary hardening from f69e896e.
///
/// `pub(crate)`: `commands::hook` normalizes the same raw hook `cwd` with
/// this exact function (see `commands::hook::lexical_capture_cwd`) before
/// joining a tool event's relative candidate path onto it, so the join lands
/// in the identical namespace as the marker directory found here — a
/// symlinked cwd (e.g. #671's `capture_drop_handles_symlinked_cwd`) then
/// matches `ignore_paths` without either side resolving the symlink.
pub(crate) fn absolute_normalized(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    lexically_normalize(&absolute)
}

/// Resolve `.`/`..` path components purely syntactically (no filesystem
/// access) — the well-known `path-clean` algorithm. A leading `..` that has
/// nothing left to pop (already at a root, or a still-relative path with no
/// preceding `Normal` component) is kept rather than dropped or erroring,
/// matching `canonicalize`'s inability to go above `/`.
fn lexically_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut stack: Vec<Component<'_>> = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match stack.last() {
                Some(Component::Normal(_)) => {
                    stack.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => stack.push(component),
            },
            other => stack.push(other),
        }
    }
    stack.into_iter().collect()
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
    parse_flag_in(&std::fs::read_to_string(file).ok()?, key)
}

/// [`parse_toml_flag`] over already-read marker text — the counterpart to
/// [`parse_key_in`], shared with [`declares_more_than_capture`] so it pays
/// for one read per key instead of re-opening the file.
fn parse_flag_in(text: &str, key: &str) -> Option<String> {
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
            Some(repo_marker.clone())
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
            Some(local)
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

    // ── #668: a capture-only marker is scope/settings-transparent ────────

    /// A nested marker whose only content is `[capture]` must not shadow an
    /// outer ancestor's declared scope: `read_scope` walks past it and
    /// returns the OUTER marker's workspace/project.
    #[test]
    fn read_scope_skips_a_nested_capture_only_marker() {
        let tmp = TempDir::new().unwrap();
        let outer_marker = write_marker(tmp.path(), "workspace = \"acme\"\nproject = \"infra\"\n");
        let inner = tmp.path().join("sub");
        fs::create_dir_all(&inner).unwrap();
        write_marker(&inner, "[capture]\nignore_paths = [\"secret/**\"]\n");

        let scope = read_scope(inner.to_str().unwrap(), &RuntimeEnv::default())
            .expect("the outer marker still declares scope");
        assert_eq!(scope.workspace.as_deref(), Some("acme"));
        assert_eq!(scope.project.as_deref(), Some("infra"));
        // `scope.path` is the marker found by the lexical (non-canonicalizing)
        // walk, so it stays in the input's namespace — compare against the raw
        // marker path, not `canonicalize()` (which would diverge on macOS's
        // /var -> /private/var symlink and Windows's \\?\ prefix).
        assert_eq!(scope.path, outer_marker);
    }

    /// When every marker in the ancestor chain is capture-only (or none
    /// exist), behavior is unchanged from before #668: `read_scope` returns
    /// `None` so the caller falls back to `DEFAULT_WORKSPACE` + repo-root.
    #[test]
    fn read_scope_still_none_when_only_capture_only_markers_exist() {
        let tmp = TempDir::new().unwrap();
        write_marker(tmp.path(), "[capture]\nignore_paths = [\"secret/**\"]\n");
        let inner = tmp.path().join("sub");
        fs::create_dir_all(&inner).unwrap();
        write_marker(&inner, "[capture]\nignore_paths = [\"other/**\"]\n");

        assert_eq!(
            read_scope(inner.to_str().unwrap(), &RuntimeEnv::default()),
            None
        );
    }

    /// A marker that declares `[briefing]` but no `workspace`/`project` is
    /// NOT capture-only — it declares a forwarded setting, so it is a
    /// resolution boundary. `read_scope` must not walk past it to an outer
    /// marker's scope, even though that marker declares one: behavior for
    /// this shape is exactly what it was before #668.
    #[test]
    fn read_scope_treats_a_briefing_only_marker_as_a_settings_boundary() {
        let tmp = TempDir::new().unwrap();
        write_marker(tmp.path(), "workspace = \"acme\"\nproject = \"infra\"\n");
        let inner = tmp.path().join("sub");
        fs::create_dir_all(&inner).unwrap();
        write_marker(&inner, "[briefing]\ninject_on_session_start = true\n");

        assert_eq!(
            read_scope(inner.to_str().unwrap(), &RuntimeEnv::default()),
            None,
            "a briefing-only marker is a settings boundary: it stops the walk \
             but declares no scope of its own"
        );
    }

    #[test]
    fn declares_more_than_capture_is_conservative() {
        assert!(!declares_more_than_capture(
            "[capture]\nignore_paths = [\"a/**\"]\n"
        ));
        assert!(!declares_more_than_capture(""));
        for text in [
            "workspace = \"acme\"\n",
            "project = \"infra\"\n",
            "project_strategy = \"repo-root\"\n",
            "drop_subagent_captures = \"true\"\n",
            "[recall]\ndefault_global = true\n",
            "[briefing]\ninject_on_session_start = true\n",
            "[briefing]\nmax_chars = 4000\n",
        ] {
            assert!(
                declares_more_than_capture(text),
                "{text} should be a settings boundary"
            );
        }
    }

    // ── #671: `absolute_normalized` must be lexical, not filesystem-real ──

    /// A `..` component is resolved purely syntactically: it must not
    /// require the path to exist, which `fs::canonicalize` would (this path
    /// is guaranteed absent). Pins the regression that made macOS's
    /// `/private/var` symlink resolution and Windows' `\\?\` prefix diverge
    /// from the raw hook-reported candidate paths.
    // Unix-only fixtures (hardcoded `/`-rooted absolute paths); the lexical
    // fold itself is platform-agnostic and exercised on Windows by the capture
    // tests once they compile.
    #[cfg(unix)]
    #[test]
    fn absolute_normalized_resolves_dotdot_without_requiring_the_path_to_exist() {
        let missing = Path::new("/definitely/does/not/exist-671/nested/../sibling");
        assert_eq!(
            absolute_normalized(missing),
            PathBuf::from("/definitely/does/not/exist-671/sibling"),
            "`..` must resolve lexically even though the path is absent \
             (fs::canonicalize would have returned Err for this)"
        );
    }

    /// A leading `..` with nothing left to pop stays literal — lexical
    /// normalization can't escape above a root, matching what
    /// `fs::canonicalize` does for `/`.
    #[cfg(unix)]
    #[test]
    fn absolute_normalized_keeps_dotdot_that_cannot_go_above_root() {
        assert_eq!(
            absolute_normalized(Path::new("/../above-root")),
            PathBuf::from("/above-root")
        );
    }

    /// The regression itself: a REAL symlinked directory must be returned
    /// as-is, with the symlink component intact, rather than resolved to
    /// its target — the behavior `fs::canonicalize` had and that diverged
    /// `marker_dir` from the runtime hook's un-canonicalized candidate
    /// paths on macOS/Windows (`ignore_paths` silently stopped matching).
    // Unix-only: creating a symlink on Windows CI needs elevated privilege.
    // The regression this guards (canonicalize resolving the symlink and
    // diverging `marker_dir` from raw runtime paths) is exercised on Linux/macOS.
    #[cfg(unix)]
    #[test]
    fn absolute_normalized_does_not_resolve_a_real_symlink() {
        let tmp = TempDir::new().unwrap();
        let real_target = tmp.path().join("real-target");
        fs::create_dir_all(&real_target).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real_target, &link).unwrap();

        let via_symlink = link.join("nested").join("..").join("file.txt");
        let normalized = absolute_normalized(&via_symlink);

        assert!(
            normalized.starts_with(&link),
            "normalized path {normalized:?} must keep the `link` component \
             rather than resolving it to {real_target:?}"
        );
        assert_eq!(normalized, link.join("file.txt"));
    }
}
