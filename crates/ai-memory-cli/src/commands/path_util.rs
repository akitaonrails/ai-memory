//! Path helpers shared by command renderers and hook capture.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

/// Create `dir` (and its parents) `0700` on Unix. On non-Unix the mode is a
/// no-op (falls back to `create_dir_all`). Idempotent: an existing
/// directory's mode is left untouched (never widened, never narrowed) to
/// avoid churning a path an operator may have set deliberately.
pub(crate) fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        if dir.is_dir() {
            return Ok(());
        }
        match std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
        {
            Ok(()) => Ok(()),
            // A concurrent caller may have created it between the check and
            // the call; treat an existing directory as success.
            Err(_) if dir.is_dir() => Ok(()),
            Err(e) => Err(e),
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// The working directory as the shell names it, when that is the same
/// directory as `physical`.
///
/// `std::env::current_dir` returns the physical path (`getcwd` resolves
/// symlinks: macOS `/var/...` comes back as `/private/var/...`), while a
/// harness reports the logical path its shell keeps in `PWD`, and hook
/// routing compares that one lexically (#2016). Paths are never
/// canonicalized for that comparison (#671), so a command that must agree
/// with the hooks takes `pwd` instead, but only when it is absolute, has no
/// `.`/`..` component, and opens the very directory `physical` does (same
/// device and inode). A stale or foreign `PWD`, or one that cannot be
/// checked, leaves `physical`, so a refusal stays a refusal. Windows keeps
/// the path a process was started in, so it has no such split and keeps
/// `physical`.
pub(crate) fn logical_dir(physical: &Path, pwd: Option<&std::ffi::OsStr>) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        use std::path::Component;
        let Some(pwd) = pwd.map(Path::new) else {
            return physical.to_path_buf();
        };
        let plain = pwd.is_absolute()
            && pwd
                .components()
                .all(|part| matches!(part, Component::RootDir | Component::Normal(_)));
        let same = |a: &Path, b: &Path| match (std::fs::metadata(a), std::fs::metadata(b)) {
            (Ok(a), Ok(b)) => a.is_dir() && a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        };
        if plain && same(pwd, physical) {
            return pwd.to_path_buf();
        }
        physical.to_path_buf()
    }
    #[cfg(not(unix))]
    {
        let _ = pwd;
        physical.to_path_buf()
    }
}

/// Resolve the user home used for agent configuration paths.
///
/// Tests and scripted wrappers can set `AI_MEMORY_HOME` to exercise the
/// platform-specific config layout without touching the real user profile.
pub(crate) fn home_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("AI_MEMORY_HOME").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(home));
    }
    dirs::home_dir()
}

/// An agent's relocated config root from its own home variable, when that
/// variable is set to a non-empty, non-whitespace value; else `None`, and the
/// caller falls back to the agent's `~/...` default.
///
/// Blank is treated as unset on purpose: an exported-but-empty variable is far
/// more often an unset shell expansion than a deliberate request to install
/// into the filesystem root. The rule lives in
/// [`ai_memory_workstream::env_dir_override`] so native session import applies
/// the same one and never reads a store the installers did not wire.
///
/// The env value comes in as a parameter so tests can exercise both branches
/// without mutating process env — which is not merely inconvenient here but
/// forbidden: `std::env::set_var` is `unsafe` under edition 2024 and this
/// workspace forbids `unsafe_code`.
pub(crate) fn agent_config_home(env_override: Option<std::ffi::OsString>) -> Option<PathBuf> {
    ai_memory_workstream::env_dir_override(env_override)
}

/// Hermes Agent's home, as Hermes resolves it: `$HERMES_HOME` when set,
/// else its platform default (`get_default_hermes_root` and
/// `_platform_default_hermes_home` in `hermes_constants.py`).
///
/// The default is `%LOCALAPPDATA%\hermes` on Windows and `~/.hermes`
/// elsewhere, with `$HERMES_DATA_DIR_SUFFIX` appended to the name. The
/// Windows default is not guesswork: the Windows installer puts the home
/// under `%LOCALAPPDATA%`, and the launcher exports `HERMES_HOME` only into
/// its own process tree, so a command that runs outside Hermes (doctor,
/// install-skills) found no home at all when it looked at `~/.hermes`.
pub(crate) fn hermes_home(env: &crate::config::RuntimeEnv, home: Option<&Path>) -> Option<PathBuf> {
    hermes_home_in(
        env.var_os("HERMES_HOME"),
        env.var_os("LOCALAPPDATA"),
        env.var_os("HERMES_DATA_DIR_SUFFIX"),
        home,
        cfg!(windows),
    )
}

/// [`hermes_home`] with the environment passed in, so tests can exercise the
/// Windows and POSIX defaults on either host.
pub(crate) fn hermes_home_in(
    env_home: Option<std::ffi::OsString>,
    local_appdata: Option<std::ffi::OsString>,
    data_dir_suffix: Option<std::ffi::OsString>,
    home: Option<&Path>,
    windows: bool,
) -> Option<PathBuf> {
    if let Some(home) = agent_config_home(env_home) {
        return Some(home);
    }
    let suffix = data_dir_suffix
        .and_then(|value| value.into_string().ok())
        .unwrap_or_default();
    if windows {
        // Hermes reads `%LOCALAPPDATA%` and falls back to the user profile's
        // `AppData\Local` when it is empty.
        let base = local_appdata
            .filter(|value| value.to_str().is_none_or(|text| !text.trim().is_empty()))
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join("AppData").join("Local")))?;
        return Some(base.join(format!("hermes{suffix}")));
    }
    home.map(|home| home.join(format!(".hermes{suffix}")))
}

/// Hermes profile ids follow `^[a-z0-9][a-z0-9_-]{0,63}$`.
pub(crate) fn is_hermes_profile_id(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() <= 64
        && bytes
            .first()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_' || *b == b'-')
}

/// Claude Code's relocated config root: `$CLAUDE_CONFIG_DIR` when set, else
/// `None`. Named alias kept so Claude call sites read for what they resolve.
pub(crate) fn claude_config_dir(env_override: Option<std::ffi::OsString>) -> Option<PathBuf> {
    agent_config_home(env_override)
}

/// Candidate Claude Code paths for uninstall. The active relocated path comes
/// first, followed by the legacy home path, with exact duplicates removed.
pub(crate) fn claude_config_paths(
    home: Option<&Path>,
    relocated: Option<&Path>,
    legacy_relative: &Path,
    relocated_relative: &Path,
) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(2);
    if let Some(root) = relocated {
        paths.push(root.join(relocated_relative));
    }
    if let Some(root) = home {
        let legacy = root.join(legacy_relative);
        if !paths.contains(&legacy) {
            paths.push(legacy);
        }
    }
    paths
}

/// `path` as shown to the operator: without a safe `\\?\` verbatim prefix,
/// which the canonical data dir carries on Windows and which `cmd` and most
/// scripts cannot use when the path is copied out of a message.
pub(crate) fn display_path(path: &Path) -> String {
    strip_windows_verbatim_prefix(&path.to_string_lossy())
        .chars()
        .map(|character| {
            if character.is_control() {
                '?'
            } else {
                character
            }
        })
        .collect()
}

/// Strip only Windows verbatim path prefixes that are safe to render as plain
/// paths. Unknown verbatim forms are left unchanged.
pub(crate) fn strip_windows_verbatim_prefix(path: &str) -> Cow<'_, str> {
    let Some(rest) = path.strip_prefix(r"\\?\") else {
        return Cow::Borrowed(path);
    };

    if rest.len() >= 3
        && rest.as_bytes()[0].is_ascii_alphabetic()
        && rest.as_bytes()[1] == b':'
        && rest.as_bytes()[2] == b'\\'
    {
        return Cow::Borrowed(rest);
    }

    let bytes = rest.as_bytes();
    if bytes.len() >= 4
        && bytes[0].eq_ignore_ascii_case(&b'U')
        && bytes[1].eq_ignore_ascii_case(&b'N')
        && bytes[2].eq_ignore_ascii_case(&b'C')
        && bytes[3] == b'\\'
    {
        return Cow::Owned(format!(r"\\{}", &rest[4..]));
    }

    Cow::Borrowed(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    /// #2016: `PWD` names the directory only when it opens the same one as
    /// the physical cwd; anything else keeps the physical path.
    #[cfg(unix)]
    #[test]
    fn logical_dir_takes_pwd_only_for_the_same_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real").join("repo");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(tmp.path().join("real"), &link).unwrap();
        let logical = link.join("repo");
        let pwd = |path: &Path| Some(path.as_os_str().to_owned());

        assert_eq!(logical_dir(&real, pwd(&logical).as_deref()), logical);
        assert_eq!(logical_dir(&real, pwd(&real).as_deref()), real);
        for refused in [
            pwd(&other),
            pwd(&link),
            pwd(&tmp.path().join("missing")),
            pwd(&link.join("repo").join("..").join("repo")),
            Some(OsString::from("link/repo")),
            None,
        ] {
            assert_eq!(logical_dir(&real, refused.as_deref()), real, "{refused:?}");
        }
        // A file with the directory's identity is impossible, but a plain file
        // never stands in for it either.
        let file = tmp.path().join("file");
        std::fs::write(&file, "").unwrap();
        assert_eq!(logical_dir(&real, pwd(&file).as_deref()), real);
    }

    #[test]
    fn claude_config_dir_honours_non_empty_value() {
        assert_eq!(
            claude_config_dir(Some(OsString::from("/stores/claude"))),
            Some(PathBuf::from("/stores/claude"))
        );
    }

    #[test]
    fn claude_config_dir_treats_unset_empty_and_whitespace_as_unset() {
        for env in [None, Some(OsString::new()), Some(OsString::from("   "))] {
            assert_eq!(claude_config_dir(env.clone()), None, "env {env:?}");
        }
    }

    /// The Windows setup puts Hermes under `%LOCALAPPDATA%\hermes` and does
    /// not export `HERMES_HOME`, so the platform default is the only way an
    /// out-of-process command finds it; POSIX keeps `~/.hermes`.
    #[test]
    fn hermes_home_models_the_platform_default() {
        let home = Path::new("/home/alice");
        assert_eq!(
            hermes_home_in(None, None, None, Some(home), false),
            Some(home.join(".hermes"))
        );
        assert_eq!(
            hermes_home_in(None, None, None, Some(home), true),
            Some(home.join("AppData").join("Local").join("hermes"))
        );
        assert_eq!(
            hermes_home_in(
                None,
                Some(r"C:\Users\alice\AppData\Local".into()),
                None,
                Some(home),
                true
            ),
            Some(PathBuf::from(r"C:\Users\alice\AppData\Local").join("hermes"))
        );
        // Hermes appends the suffix to the default's name, never to
        // `$HERMES_HOME`.
        assert_eq!(
            hermes_home_in(None, None, Some("_dev".into()), Some(home), false),
            Some(home.join(".hermes_dev"))
        );
        assert_eq!(
            hermes_home_in(
                Some("/srv/hermes".into()),
                None,
                Some("_dev".into()),
                Some(home),
                true
            ),
            Some(PathBuf::from("/srv/hermes"))
        );
        // An exported-but-empty or whitespace value is an unset expansion.
        for unset in [None, Some(std::ffi::OsString::new()), Some("  ".into())] {
            assert_eq!(
                hermes_home_in(unset, None, None, Some(home), false),
                Some(home.join(".hermes"))
            );
        }
        assert_eq!(hermes_home_in(None, None, None, None, false), None);
        assert_eq!(hermes_home_in(None, None, None, None, true), None);
    }

    #[test]
    fn claude_config_paths_put_relocated_first_and_deduplicate() {
        assert_eq!(
            claude_config_paths(
                Some(Path::new("/home/alice")),
                Some(Path::new("/stores/claude")),
                Path::new(".claude/settings.json"),
                Path::new("settings.json"),
            ),
            [
                PathBuf::from("/stores/claude/settings.json"),
                PathBuf::from("/home/alice/.claude/settings.json"),
            ]
        );
        assert_eq!(
            claude_config_paths(
                Some(Path::new("/home/alice")),
                Some(Path::new("/home/alice/.claude")),
                Path::new(".claude/settings.json"),
                Path::new("settings.json"),
            ),
            [PathBuf::from("/home/alice/.claude/settings.json")]
        );
    }

    #[test]
    fn display_path_drops_the_verbatim_prefix() {
        assert_eq!(
            display_path(Path::new(
                r"\\?\C:\Users\me\AppData\Local\ai-memory\auth.json"
            )),
            r"C:\Users\me\AppData\Local\ai-memory\auth.json"
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\ai-memory")),
            r"\\server\share\ai-memory"
        );
        assert_eq!(
            display_path(Path::new("/home/alice/.local/share/ai-memory")),
            "/home/alice/.local/share/ai-memory"
        );
        assert_eq!(
            display_path(Path::new("/tmp/line\nbreak")),
            "/tmp/line?break"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_handles_drive_paths() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\C:\Users\me\AppData\Local\ai-memory"),
            r"C:\Users\me\AppData\Local\ai-memory"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\c:\Users\me\AppData\Local\ai-memory"),
            r"c:\Users\me\AppData\Local\ai-memory"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_handles_unc_case_insensitively() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\UNC\server\share\m"),
            r"\\server\share\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\unc\Server\Share\m"),
            r"\\Server\Share\m"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_leaves_non_verbatim_paths_untouched() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"C:\Users\me\ai-memory"),
            r"C:\Users\me\ai-memory"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix("/home/alice/.local/share/ai-memory"),
            "/home/alice/.local/share/ai-memory"
        );
    }

    #[test]
    fn strip_windows_verbatim_prefix_leaves_unknown_forms_untouched() {
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\m"),
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\GLOBALROOT\Device\HarddiskVolume1\m"),
            r"\\?\GLOBALROOT\Device\HarddiskVolume1\m"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\.\PhysicalDrive0"),
            r"\\.\PhysicalDrive0"
        );
        assert_eq!(
            &*strip_windows_verbatim_prefix(r"\\?\C:relative\m"),
            r"\\?\C:relative\m"
        );
    }
}
